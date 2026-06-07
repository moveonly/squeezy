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

fn registry_with_state_store_and_checkpoints(
    root: &Path,
    store: Arc<SqueezyStore>,
) -> ToolRegistry {
    ToolRegistry::new_with_configs_skills_and_mcp(
        root,
        ToolRuntimeConfig {
            checkpoints_enabled: true,
            ..ToolRuntimeConfig::default()
        },
        SkillsConfig::default(),
        &GraphConfig::default(),
        ToolRegistryRuntime::new(Some(store), Arc::new(Redactor::default())),
    )
    .expect("registry")
}

#[test]
fn plan_parallel_batches_coalesces_three_delegate_calls_into_one_concurrent_batch() {
    // F10: three consecutive `delegate*` calls in the same model turn must
    // land in a single parallel batch so the dispatcher's
    // `buffer_unordered(SUBAGENT_MAX_CONCURRENT)` loop can run them
    // concurrently. Before this change `is_parallel_safe` consulted only
    // the spec catalog, which has no entry for the synthetic subagent
    // tools — so the lease pool's concurrent budget was never used by the
    // dispatcher.
    //
    // The mix below intentionally covers all three delegate variants
    // (plain delegate, plan, review) plus `delegate_chain`, which is also
    // parallel-safe at the registry level because its body runs an
    // internal step sequence and looks like any other read-only synthetic
    // tool to the dispatcher.
    let root = temp_workspace("plan_parallel_batches_delegates");
    let registry = registry_with_shell_sandbox_off(&root);

    let make_call = |id: &str, name: &str, args: Value| ToolCall {
        call_id: id.to_string(),
        name: name.to_string(),
        arguments: args,
    };
    let calls = vec![
        make_call("d1", "delegate", json!({"prompt": "map module A"})),
        make_call("d2", "delegate_plan", json!({"goal": "add tracing"})),
        make_call("d3", "delegate_review", json!({"scope": "src/"})),
    ];

    for call in &calls {
        assert!(
            registry.is_parallel_safe(call),
            "synthetic delegate tool `{}` must be marked parallel_safe so concurrent dispatch is unlocked",
            call.name
        );
    }
    assert!(registry.is_parallel_safe(&ToolCall {
        call_id: "chain".to_string(),
        name: "delegate_chain".to_string(),
        arguments: json!({"steps": [{"prompt": "A"}, {"prompt": "B"}]}),
    }));

    let batches = registry.plan_parallel_batches(&calls);

    assert_eq!(
        batches,
        vec![ParallelExecutionBatch {
            indices: vec![0, 1, 2],
            parallel_safe: true,
        }],
        "three sibling delegate calls must coalesce into one concurrent batch"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn plan_parallel_batches_coalesces_consecutive_parallel_safe_calls() {
    // Shape the input as a real model turn that emits read-only
    // navigation calls back-to-back. Every spec involved here is
    // `parallel_safe: true`, so the planner must produce a single
    // batch covering all indices so the dispatcher's
    // `buffer_unordered` loop can run them concurrently instead of
    // serializing the run.
    let root = temp_workspace("plan_parallel_batches_safe");
    let registry = registry_with_shell_sandbox_off(&root);

    let make_call = |id: &str, name: &str, args: Value| ToolCall {
        call_id: id.to_string(),
        name: name.to_string(),
        arguments: args,
    };
    let calls = vec![
        make_call("c1", "read_file", json!({"path": "Cargo.toml"})),
        make_call("c2", "grep", json!({"pattern": "fn main"})),
        make_call("c3", "decl_search", json!({"query": "main"})),
        make_call("c4", "symbol_context", json!({"query": "main"})),
    ];

    let batches = registry.plan_parallel_batches(&calls);

    assert_eq!(
        batches,
        vec![ParallelExecutionBatch {
            indices: vec![0, 1, 2, 3],
            parallel_safe: true,
        }],
        "all-parallel-safe calls must coalesce into one concurrent batch"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn plan_parallel_batches_serializes_unsafe_calls_between_safe_runs() {
    // Mix unsafe calls (shell, apply_patch) between read-only ones to
    // confirm each unsafe call splits the surrounding parallel run.
    // Each unsafe call must land alone in its own batch, and the
    // contiguous safe runs on either side must stay coalesced.
    let root = temp_workspace("plan_parallel_batches_mixed");
    let registry = registry_with_shell_sandbox_off(&root);

    let make_call = |id: &str, name: &str, args: Value| ToolCall {
        call_id: id.to_string(),
        name: name.to_string(),
        arguments: args,
    };
    let calls = vec![
        make_call("c0", "read_file", json!({"path": "Cargo.toml"})),
        make_call("c1", "grep", json!({"pattern": "fn main"})),
        make_call(
            "c2",
            "shell",
            json!({"command": "echo hi", "description": "smoke"}),
        ),
        make_call("c3", "read_slice", json!({"path": "src/lib.rs"})),
        make_call(
            "c4",
            "apply_patch",
            json!({"operations": [{"kind": "create_file", "path": "notes.txt", "contents": "x"}]}),
        ),
        make_call("c5", "decl_search", json!({"query": "main"})),
        make_call("c6", "symbol_context", json!({"query": "main"})),
    ];

    let batches = registry.plan_parallel_batches(&calls);

    assert_eq!(
        batches,
        vec![
            ParallelExecutionBatch {
                indices: vec![0, 1],
                parallel_safe: true,
            },
            ParallelExecutionBatch {
                indices: vec![2],
                parallel_safe: false,
            },
            ParallelExecutionBatch {
                indices: vec![3],
                parallel_safe: true,
            },
            ParallelExecutionBatch {
                indices: vec![4],
                parallel_safe: false,
            },
            ParallelExecutionBatch {
                indices: vec![5, 6],
                parallel_safe: true,
            },
        ],
        "unsafe calls must land in singleton batches that split the surrounding parallel runs"
    );

    // The dispatcher consults is_parallel_safe at execution time. The
    // matching property here documents that the planner's batch flag
    // tracks the per-call lookup the dispatcher uses to flush vs.
    // continue a pending parallel run.
    for batch in &batches {
        let expected = batch.parallel_safe;
        for &idx in &batch.indices {
            assert_eq!(
                registry.is_parallel_safe(&calls[idx]),
                expected,
                "is_parallel_safe must agree with the planner batch flag for {}",
                calls[idx].name
            );
        }
    }

    let _ = fs::remove_dir_all(root);
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

#[tokio::test]
async fn outside_workspace_paths_require_permission_grant_unless_full_access() {
    let root = temp_workspace("outside_workspace_permission");
    let outside = root
        .parent()
        .expect("temp workspace has parent")
        .join(format!(
            "{}-outside.txt",
            root.file_name().unwrap().to_string_lossy()
        ));
    let _ = fs::remove_file(&outside);

    let registry = registry_with_shell_sandbox_off(&root);
    let write_call = ToolCall {
        call_id: "outside-write".to_string(),
        name: "write_file".to_string(),
        arguments: json!({
            "path": outside.display().to_string(),
            "content": "outside\n",
        }),
    };
    let request = registry.permission_request(&write_call);
    assert_eq!(request.capability, PermissionCapability::Edit);
    assert_eq!(request.target, format!("path:{}", outside.display()));
    assert_eq!(request.metadata["outside_workspace"], "true");

    let denied = registry
        .execute(write_call.clone(), CancellationToken::new())
        .await;
    assert_eq!(denied.status, ToolStatus::Denied);

    registry.record_permission_grant(&request);
    let allowed = registry.execute(write_call, CancellationToken::new()).await;
    assert_eq!(allowed.status, ToolStatus::Success);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "outside\n");

    let read_call = ToolCall {
        call_id: "outside-write".to_string(),
        name: "read_file".to_string(),
        arguments: json!({
            "path": outside.display().to_string(),
        }),
    };
    let read_request = registry.permission_request(&read_call);
    assert_eq!(read_request.capability, PermissionCapability::Read);
    assert_eq!(read_request.target, format!("path:{}", outside.display()));
    assert_eq!(read_request.metadata["outside_workspace"], "true");

    let denied_read = registry
        .execute(read_call.clone(), CancellationToken::new())
        .await;
    assert_eq!(denied_read.status, ToolStatus::Error);

    registry.record_permission_grant(&read_request);
    let allowed_read = registry
        .execute(read_call.clone(), CancellationToken::new())
        .await;
    assert_eq!(allowed_read.status, ToolStatus::Success);

    let full_access = registry_with_runtime_config(
        &root,
        ToolRuntimeConfig {
            full_access: true,
            ..ToolRuntimeConfig::default()
        },
    );
    let read = full_access
        .execute(
            ToolCall {
                call_id: "outside-read".to_string(),
                name: "read_file".to_string(),
                arguments: json!({
                    "path": outside.display().to_string(),
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(read.status, ToolStatus::Success);

    let _ = fs::remove_file(outside);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn session_approval_extends_to_edit_family_on_optin() {
    // F04-permission-scope-collapse-edit-family: once the
    // user opts into a session rule from one edit-family tool on a given
    // path, the same rule must cover the rest of the family (write_file
    // <-> apply_patch) for that path without re-prompting. The mechanism
    // is the shared capability ("edit") + target ("path:<file>") shape
    // both arms register; this test pins that contract so any future
    // refactor that drifts apply_patch or write_file off `path:<file>`
    // fails loudly.
    use squeezy_core::{PermissionAction, PermissionPolicy};
    let root = temp_workspace("permission_edit_family_optin");
    let registry = registry_with_shell_sandbox_off(&root);
    let policy = PermissionPolicy {
        edit: PermissionMode::Ask,
        ..PermissionPolicy::default()
    };

    let write_request = registry.permission_request(&ToolCall {
        call_id: "write".to_string(),
        name: "write_file".to_string(),
        arguments: json!({
            "path": "src/foo.rs",
            "content": "fn main() {}",
            "expected_sha256": "deadbeef",
        }),
    });
    let patch_request = registry.permission_request(&ToolCall {
        call_id: "patch".to_string(),
        name: "apply_patch".to_string(),
        arguments: json!({
            "patches": [{
                "path": "src/foo.rs",
                "search": "fn main() {}",
                "replace": "fn main() { println!(\"hi\"); }",
            }],
        }),
    });
    assert_eq!(
        patch_request.target, write_request.target,
        "apply_patch and write_file on the same path must produce identical \
         permission targets so a session rule from one covers the other",
    );

    // Default Ask + no rules: every edit-family call must still prompt.
    let baseline_write = policy.evaluate(&write_request);
    let baseline_patch = policy.evaluate(&patch_request);
    assert_eq!(baseline_write.action, PermissionAction::Ask);
    assert_eq!(baseline_patch.action, PermissionAction::Ask);

    // After the user picks AllowSession on the write_file prompt, that
    // single suggested rule must extend to the apply_patch sibling.
    let write_session_rule = write_request
        .suggested_rules
        .first()
        .expect("write_file should suggest a session rule")
        .clone();
    assert_eq!(write_session_rule.capability, "edit");
    assert_eq!(write_session_rule.source, PermissionRuleSource::Session);
    let extended_patch =
        policy.evaluate_with_extra(&patch_request, std::slice::from_ref(&write_session_rule));
    assert_eq!(
        extended_patch.action,
        PermissionAction::Allow,
        "approving write_file for path:src/foo.rs must auto-allow apply_patch \
         on the same path",
    );
    assert_eq!(
        extended_patch
            .matched_rule
            .as_ref()
            .map(|rule| rule.target.as_str()),
        Some("path:src/foo.rs"),
    );

    // And the reverse: a session rule installed via apply_patch must
    // cover a subsequent write_file on the same path. Without this, the
    // user has to approve each tool variant separately, defeating the
    // edit-family collapse.
    let patch_session_rule = patch_request
        .suggested_rules
        .first()
        .expect("apply_patch should suggest a session rule")
        .clone();
    assert_eq!(patch_session_rule.capability, "edit");
    assert_eq!(patch_session_rule.target, "path:src/foo.rs");
    let extended_write =
        policy.evaluate_with_extra(&write_request, std::slice::from_ref(&patch_session_rule));
    assert_eq!(
        extended_write.action,
        PermissionAction::Allow,
        "approving apply_patch for path:src/foo.rs must auto-allow write_file \
         on the same path",
    );

    // Opt-in narrowness: the rule is path-scoped, so a sibling path is
    // still gated. This pins that the collapse never widens beyond the
    // approved target.
    let other_patch = registry.permission_request(&ToolCall {
        call_id: "patch_other".to_string(),
        name: "apply_patch".to_string(),
        arguments: json!({
            "patches": [{
                "path": "src/bar.rs",
                "search": "x",
                "replace": "y",
            }],
        }),
    });
    let other_verdict =
        policy.evaluate_with_extra(&other_patch, std::slice::from_ref(&write_session_rule));
    assert_eq!(other_verdict.action, PermissionAction::Ask);

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

#[test]
fn apply_patch_summary_and_metadata_walk_operations_shape() {
    // `apply_patch` accepts both `patches[]` and `operations[]`; the
    // approval summary line and the `paths` metadata entry must walk
    // both shapes so the reviewer always sees which files are about to
    // change — covering create_file, delete_file, search_replace, and
    // both endpoints of move_file. Paths must be deduped and sorted so
    // the rendered summary is stable across permutations of the input.
    let root = temp_workspace("apply_patch_operations_summary");
    let registry = ToolRegistry::new(&root).expect("registry");

    let call = ToolCall {
        call_id: "ops".to_string(),
        name: "apply_patch".to_string(),
        arguments: json!({
            "operations": [
                {
                    "kind": "create_file",
                    "path": "crates/squeezy-eval/README-PROBE.md",
                    "contents": "# Probe one\n",
                },
                {
                    "kind": "create_file",
                    "path": "crates/squeezy-eval/README-PROBE2.md",
                    "contents": "# Probe two\n",
                },
                {
                    "kind": "delete_file",
                    "path": "crates/squeezy-eval/OLD.md",
                },
                {
                    "kind": "search_replace",
                    "path": "crates/squeezy-eval/lib.rs",
                    "search": "fn old()",
                    "replace": "fn new()",
                },
                {
                    "kind": "move_file",
                    "from": "src/old_name.rs",
                    "to": "src/new_name.rs",
                },
            ],
        }),
    };

    let description = registry.describe_call(&call);
    assert_eq!(
        description,
        "apply_patch paths=\"crates/squeezy-eval/OLD.md, \
         crates/squeezy-eval/README-PROBE.md, \
         crates/squeezy-eval/README-PROBE2.md, \
         crates/squeezy-eval/lib.rs, \
         src/new_name.rs, \
         src/old_name.rs\"",
        "summary must list every path touched by operations[] (including \
         both endpoints of move_file), deduped and sorted",
    );

    let request = registry.permission_request(&call);
    assert_eq!(
        request.metadata["paths"],
        "crates/squeezy-eval/OLD.md, \
         crates/squeezy-eval/README-PROBE.md, \
         crates/squeezy-eval/README-PROBE2.md, \
         crates/squeezy-eval/lib.rs, \
         src/new_name.rs, \
         src/old_name.rs",
        "approval metadata `paths` must mirror the summary so the audit \
         line and the metadata stay in sync",
    );
    // Multi-path operations land on the generic workspace target rather
    // than collapsing to a single file path.
    assert_eq!(request.target, "workspace:patches");
    // The first five paths should each register a session rule so the
    // reviewer's "allow this path" choice survives across the call.
    let rule_targets: Vec<&str> = request
        .suggested_rules
        .iter()
        .map(|rule| rule.target.as_str())
        .collect();
    assert!(
        rule_targets.contains(&"path:crates/squeezy-eval/README-PROBE.md"),
        "suggested rules must seed at least the create_file paths; got {rule_targets:?}",
    );

    // Single-op operations[] payload collapses to `path:<that>` so the
    // session rule from this approval covers the same path for future
    // edit-family calls (matching the legacy single-patch behaviour).
    let single = registry.permission_request(&ToolCall {
        call_id: "single".to_string(),
        name: "apply_patch".to_string(),
        arguments: json!({
            "operations": [{
                "kind": "create_file",
                "path": "src/only.rs",
                "contents": "fn main() {}\n",
            }],
        }),
    });
    assert_eq!(single.target, "path:src/only.rs");
    assert_eq!(single.metadata["paths"], "src/only.rs");

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
async fn grep_rejects_unknown_field() {
    let root = temp_workspace("grep_unknown_field");
    fs::write(root.join("visible.txt"), "needle\n").expect("write visible");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_unknown".to_string(),
                name: "grep".to_string(),
                arguments: json!({"patern": "needle"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    let message = result.content["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("unknown field"),
        "expected serde unknown-field error, got: {message}"
    );
    assert!(
        message.contains("patern"),
        "expected error to mention misspelled key, got: {message}"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn grep_exclude_filter_drops_matching_paths() {
    let root = temp_workspace("grep_exclude");
    fs::write(root.join("a.rs"), "needle\n").expect("write a");
    fs::write(root.join("b.rs"), "needle\n").expect("write b");
    fs::write(root.join("c.rs"), "needle\n").expect("write c");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_exclude".to_string(),
                name: "grep".to_string(),
                arguments: json!({
                    "pattern": "needle",
                    "exclude": ["**/b.rs"],
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let mut paths = match_paths(&result);
    paths.sort();
    assert_eq!(
        paths,
        vec!["a.rs".to_string(), "c.rs".to_string()],
        "exclude=**/b.rs must skip b.rs while keeping a.rs and c.rs"
    );
    assert_eq!(result.content["metadata"]["exclude"], json!(["**/b.rs"]));

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
async fn grep_context_two_emits_five_line_window_around_match() {
    let root = temp_workspace("grep_context_two");
    fs::write(
        root.join("notes.txt"),
        "alpha\nbeta\ngamma\nneedle\ndelta\nepsilon\nzeta\n",
    )
    .expect("write notes");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_context".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "needle", "context": 2}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["metadata"]["context"], json!(2));
    let matches = result.content["matches"].as_array().expect("matches");
    assert_eq!(matches.len(), 1);

    let entry = &matches[0];
    assert_eq!(entry["path"], json!("notes.txt"));
    assert_eq!(entry["line"], json!(4));
    assert_eq!(entry["text"], json!("needle"));

    let before = entry["context_before"].as_array().expect("context_before");
    let after = entry["context_after"].as_array().expect("context_after");
    assert_eq!(before.len(), 2);
    assert_eq!(after.len(), 2);
    let window_len = before.len() + 1 + after.len();
    assert_eq!(window_len, 5, "context=2 must emit a 5-line window");

    assert_eq!(before[0], json!({"line": 2, "text": "beta"}));
    assert_eq!(before[1], json!({"line": 3, "text": "gamma"}));
    assert_eq!(after[0], json!({"line": 5, "text": "delta"}));
    assert_eq!(after[1], json!({"line": 6, "text": "epsilon"}));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn grep_context_zero_preserves_pre_f13_match_shape() {
    let root = temp_workspace("grep_context_zero");
    fs::write(
        root.join("notes.txt"),
        "alpha\nbeta\nneedle\ndelta\nepsilon\n",
    )
    .expect("write notes");
    let registry = ToolRegistry::new(&root).expect("registry");

    let explicit_zero = registry
        .execute(
            ToolCall {
                call_id: "call_zero".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "needle", "context": 0}),
            },
            CancellationToken::new(),
        )
        .await;
    let default = registry
        .execute(
            ToolCall {
                call_id: "call_default".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "needle"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(explicit_zero.status, ToolStatus::Success);
    assert_eq!(default.status, ToolStatus::Success);
    assert_eq!(explicit_zero.content["metadata"]["context"], json!(0));
    assert_eq!(default.content["metadata"]["context"], json!(0));
    assert_eq!(
        explicit_zero.content["matches"], default.content["matches"],
        "context=0 must match the omitted-arg default"
    );

    let matches = explicit_zero.content["matches"]
        .as_array()
        .expect("matches");
    assert_eq!(matches.len(), 1);
    let entry = &matches[0];
    assert_eq!(entry["path"], json!("notes.txt"));
    assert_eq!(entry["line"], json!(3));
    assert_eq!(entry["text"], json!("needle"));
    assert!(
        entry.get("context_before").is_none(),
        "context=0 must not emit context_before; got: {entry}"
    );
    assert!(
        entry.get("context_after").is_none(),
        "context=0 must not emit context_after; got: {entry}"
    );

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
    assert_eq!(result.content["content"], "1\tcde");
    assert_eq!(result.content["start_line"], 1);
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
async fn read_file_returns_image_payload_when_file_is_png() {
    use base64::Engine as _;
    let root = temp_workspace("read_file_image");
    // Synthetic PNG: 8-byte magic header followed by a minimal IHDR-like
    // payload. The bytes don't have to form a renderable image — the
    // tool only inspects magic bytes for MIME detection.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    bytes.extend_from_slice(b"synthetic-image-body");
    fs::write(root.join("logo.png"), &bytes).expect("write png");

    let registry = ToolRegistry::new(&root).expect("registry");
    let result = registry
        .execute(
            ToolCall {
                call_id: "read_image".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "logo.png"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["image"], true);
    assert_eq!(result.content["mime_type"], "image/png");
    assert!(
        result.content.get("content").is_none(),
        "image payload must not include raw text `content`: {:?}",
        result.content,
    );
    let encoded = result.content["data_base64"]
        .as_str()
        .expect("base64 string");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("valid base64");
    assert_eq!(decoded, bytes);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_file_rejects_image_exceeding_size_cap() {
    use crate::file_ops::MAX_IMAGE_BYTES;
    let root = temp_workspace("read_file_image_too_large");
    let size: usize = 6 * 1024 * 1024;
    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    bytes.resize(size, 0u8);
    fs::write(root.join("huge.png"), &bytes).expect("write png");

    let registry = ToolRegistry::new(&root).expect("registry");
    let result = registry
        .execute(
            ToolCall {
                call_id: "read_huge_image".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "huge.png"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(result.content["image"], true);
    assert_eq!(result.content["mime_type"], "image/png");
    assert_eq!(result.content["total_bytes"], size as u64);
    assert_eq!(result.content["max_image_bytes"], MAX_IMAGE_BYTES);
    assert_eq!(result.content["path"], "huge.png");
    let err_msg = result.content["error"].as_str().expect("error string");
    assert!(err_msg.contains("huge.png"), "error msg: {err_msg}");
    assert!(err_msg.contains(&size.to_string()), "error msg: {err_msg}",);
    assert!(
        err_msg.contains(&MAX_IMAGE_BYTES.to_string()),
        "error msg: {err_msg}",
    );
    assert!(
        result.content.get("data_base64").is_none(),
        "rejected image must not include base64: {:?}",
        result.content,
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
    assert_eq!(result.content["content"], "1\talpha\n2\tDELTA\n3\tgamma\n");
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
    assert_eq!(result.content["content"], "2\tbeta");
    assert_eq!(result.content["start_line"], 2);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn grep_returns_resident_receipt_when_file_already_read_in_full() {
    let root = temp_workspace("grep_resident_receipt");
    let body = "alpha fox\nbeta\nalpha bird\n";
    fs::write(root.join("sample.txt"), body).expect("write sample");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    // A full-file read_file snapshot, stored in the model-facing
    // line-numbered render that `prefix_lines_with_numbers` produces.
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "sample.txt".to_string(),
            tool_name: "read_file".to_string(),
            call_id: "prior_read".to_string(),
            stable_output_sha256: "prior-output".to_string(),
            content_sha256: Some(sha256_hex(body.as_bytes())),
            start_byte: 0,
            end_byte: body.len() as u64,
            content: "1\talpha fox\n2\tbeta\n3\talpha bird\n".to_string(),
            model_output_bytes: 256,
            created_unix_millis: 1,
        })
        .expect("put snapshot");
    let registry = registry_with_state_store(&root, store);

    let result = registry
        .execute(
            ToolCall {
                call_id: "grep_call".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "alpha", "path": "sample.txt"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let metadata = result.content["metadata"]
        .as_object()
        .expect("metadata object");
    assert_eq!(metadata["receipt_stub"], true);
    assert_eq!(metadata["dedup"], true);
    assert_eq!(metadata["resident_read"], true);
    assert_eq!(metadata["same_as_call_id"], "prior_read");
    assert_eq!(metadata["same_as_tool_name"], "read_file");

    let matches = result.content["matches"].as_array().expect("matches array");
    assert_eq!(matches.len(), 2);
    // Matched line numbers come from the embedded gutter, and the emitted
    // text is the source line WITHOUT the "{N}\t" gutter — never the full
    // file source.
    assert_eq!(matches[0]["line"], 1);
    assert_eq!(matches[0]["text"], "alpha fox");
    assert_eq!(matches[1]["line"], 3);
    assert_eq!(matches[1]["text"], "alpha bird");
    // The receipt must not re-emit the full file content.
    assert!(result.content.get("content").is_none());
    let serialized = serde_json::to_string(&result.content).expect("serialize");
    assert!(
        !serialized.contains("beta"),
        "non-matching line leaked into the receipt: {serialized}"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn grep_resident_receipt_matches_equal_disk_grep() {
    let root = temp_workspace("grep_resident_equals_disk");
    let body = "alpha fox\nbeta\nalpha bird\n";
    fs::write(root.join("sample.txt"), body).expect("write sample");

    // Ground truth: a normal disk grep with no resident snapshot.
    let disk_registry = ToolRegistry::new(&root).expect("registry");
    let disk = disk_registry
        .execute(
            ToolCall {
                call_id: "disk_grep".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "alpha", "path": "sample.txt"}),
            },
            CancellationToken::new(),
        )
        .await;
    let disk_matches = disk.content["matches"].as_array().expect("disk matches");

    // Resident path: same file, same regex, served from the snapshot.
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "sample.txt".to_string(),
            tool_name: "read_file".to_string(),
            call_id: "prior_read".to_string(),
            stable_output_sha256: "prior-output".to_string(),
            content_sha256: Some(sha256_hex(body.as_bytes())),
            start_byte: 0,
            end_byte: body.len() as u64,
            content: "1\talpha fox\n2\tbeta\n3\talpha bird\n".to_string(),
            model_output_bytes: 256,
            created_unix_millis: 1,
        })
        .expect("put snapshot");
    let registry = registry_with_state_store(&root, store);
    let resident = registry
        .execute(
            ToolCall {
                call_id: "resident_grep".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "alpha", "path": "sample.txt"}),
            },
            CancellationToken::new(),
        )
        .await;
    let resident_matches = resident.content["matches"]
        .as_array()
        .expect("resident matches");

    assert_eq!(disk_matches.len(), resident_matches.len());
    for (disk_match, resident_match) in disk_matches.iter().zip(resident_matches.iter()) {
        assert_eq!(disk_match["line"], resident_match["line"]);
        assert_eq!(disk_match["text"], resident_match["text"]);
        assert_eq!(disk_match["path"], resident_match["path"]);
    }
    // Disk grep must NOT carry the resident-receipt markers; the resident
    // path must.
    assert!(disk.content["metadata"].get("resident_read").is_none());
    assert_eq!(resident.content["metadata"]["resident_read"], true);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn grep_falls_through_to_disk_when_snapshot_missing_stale_or_partial() {
    let body = "alpha fox\nbeta\nalpha bird\n";

    // (1) No snapshot at all -> normal disk grep, no resident markers.
    let root = temp_workspace("grep_resident_missing");
    fs::write(root.join("sample.txt"), body).expect("write sample");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    let registry = registry_with_state_store(&root, store);
    let result = registry
        .execute(
            ToolCall {
                call_id: "grep_missing".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "alpha", "path": "sample.txt"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);
    assert!(result.content["metadata"].get("resident_read").is_none());
    assert_eq!(result.content["matches"].as_array().unwrap().len(), 2);
    let _ = fs::remove_dir_all(root);

    // (2) Stale SHA (file changed since the read) -> fall through to disk.
    let root = temp_workspace("grep_resident_stale");
    fs::write(root.join("sample.txt"), body).expect("write sample");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "sample.txt".to_string(),
            tool_name: "read_file".to_string(),
            call_id: "prior_read".to_string(),
            stable_output_sha256: "prior-output".to_string(),
            // Hash of some OTHER content -> SHA mismatch.
            content_sha256: Some(sha256_hex("alpha fox\nbeta\ngamma\n".as_bytes())),
            start_byte: 0,
            end_byte: body.len() as u64,
            content: "1\talpha fox\n2\tbeta\n3\tgamma\n".to_string(),
            model_output_bytes: 256,
            created_unix_millis: 1,
        })
        .expect("put snapshot");
    let registry = registry_with_state_store(&root, store);
    let result = registry
        .execute(
            ToolCall {
                call_id: "grep_stale".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "alpha", "path": "sample.txt"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);
    assert!(result.content["metadata"].get("resident_read").is_none());
    // Disk grep sees the real file: "alpha bird" on line 3, not "gamma".
    let matches = result.content["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[1]["text"], "alpha bird");
    let _ = fs::remove_dir_all(root);

    // (3) Partial-span snapshot (covers only the first line) -> fall through.
    let root = temp_workspace("grep_resident_partial");
    fs::write(root.join("sample.txt"), body).expect("write sample");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "sample.txt".to_string(),
            tool_name: "read_file".to_string(),
            call_id: "prior_read".to_string(),
            stable_output_sha256: "prior-output".to_string(),
            content_sha256: Some(sha256_hex(body.as_bytes())),
            start_byte: 0,
            // Only the first line of the file is covered.
            end_byte: 10,
            content: "1\talpha fox\n".to_string(),
            model_output_bytes: 64,
            created_unix_millis: 1,
        })
        .expect("put snapshot");
    let registry = registry_with_state_store(&root, store);
    let result = registry
        .execute(
            ToolCall {
                call_id: "grep_partial".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "alpha", "path": "sample.txt"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);
    assert!(result.content["metadata"].get("resident_read").is_none());
    // Disk grep finds BOTH matches, proving we did not stop at the
    // partial snapshot's single covered line.
    assert_eq!(result.content["matches"].as_array().unwrap().len(), 2);
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
async fn symbol_context_reports_truncated_when_matches_exceed_max_results() {
    // Bug #10: `symbol_context` applied `take(max_results)` but always emitted
    // `truncated:false`. With three matching symbols and `max_results:1`, the
    // response must own up to truncation; raising the cap above the match count
    // must report `false`.
    let root = temp_workspace("symbol_context_truncated");
    write_rust_crate(
        &root,
        "pub fn handler_one() {}\npub fn handler_two() {}\npub fn handler_three() {}\n",
    );
    let registry = ToolRegistry::new(&root).expect("registry");

    let truncated = registry
        .execute(
            ToolCall {
                call_id: "ctx_trunc".to_string(),
                name: "symbol_context".to_string(),
                arguments: json!({"query": "handler", "max_results": 1}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(truncated.status, ToolStatus::Success);
    assert_eq!(
        truncated.content["packets"]
            .as_array()
            .expect("packets")
            .len(),
        1,
        "max_results=1 must return a single packet"
    );
    assert_eq!(
        truncated.content["truncated"], true,
        "more matches than max_results must report truncated:true"
    );

    let complete = registry
        .execute(
            ToolCall {
                call_id: "ctx_full".to_string(),
                name: "symbol_context".to_string(),
                arguments: json!({"query": "handler", "max_results": 10}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(complete.status, ToolStatus::Success);
    assert_eq!(
        complete.content["truncated"], false,
        "fewer matches than max_results must report truncated:false"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_slice_diff_reports_truncated_when_single_hunk_exceeds_max_ranges() {
    // Bug #12: a single hunk can hold many changed ranges. `take(max_ranges)`
    // dropped the surplus but `truncated` only checked hunk count, so a
    // many-range single-hunk diff reported `truncated:false`. Stage a file with
    // several interleaved changed/unchanged lines (one contiguous hunk, many
    // changed ranges) and cap `max_ranges` below the range count.
    let root = temp_workspace("read_slice_diff_ranges_truncated");
    let original = "a0\nKEEP\na1\nKEEP\na2\nKEEP\na3\nKEEP\na4\nKEEP\n";
    write_rust_crate(&root, "pub fn placeholder() {}\n");
    fs::write(root.join("data.txt"), original).expect("write original");
    git_init_commit(&root);
    // Flip every "aN" line; the unchanged "KEEP" lines between them split the
    // edit into many distinct changed byte ranges inside one diff hunk.
    let modified = "b0\nKEEP\nb1\nKEEP\nb2\nKEEP\nb3\nKEEP\nb4\nKEEP\n";
    fs::write(root.join("data.txt"), modified).expect("write modified");
    let registry = ToolRegistry::new(&root).expect("registry");

    // Baseline: with a generous cap, the single hunk yields more than two
    // distinct changed ranges (the "KEEP" lines split the edit). This anchors
    // the "more ranges than max_ranges" precondition the bug hinges on.
    let uncapped = registry
        .execute(
            ToolCall {
                call_id: "diff_uncapped".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({
                    "path": "data.txt",
                    "read_mode": "diff",
                    "diff_baseline": "worktree",
                    "max_ranges": 100,
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(uncapped.status, ToolStatus::Success);
    let uncapped_ranges = uncapped.content["ranges"].as_array().expect("ranges");
    assert!(
        uncapped_ranges.len() > 2,
        "precondition: a single hunk must yield more than max_ranges ranges; got {}",
        uncapped_ranges.len()
    );

    let result = registry
        .execute(
            ToolCall {
                call_id: "diff".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({
                    "path": "data.txt",
                    "read_mode": "diff",
                    "diff_baseline": "worktree",
                    "max_ranges": 2,
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let ranges = result.content["ranges"].as_array().expect("ranges");
    assert_eq!(
        ranges.len(),
        2,
        "ranges must be capped at max_ranges=2, got {}",
        ranges.len()
    );
    assert_eq!(
        result.content["truncated"], true,
        "dropping ranges within a single hunk must report truncated:true"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_slice_diff_mode_reports_policy_ignored_metadata() {
    // Bug #15: slice mode surfaced policy-exclusion metadata
    // (`ignored`/`ignored_reason`) but diff mode dropped it, so a model could
    // not tell a policy-excluded file apart from a clean one in diff mode.
    let root = temp_workspace("read_slice_diff_ignored");
    fs::create_dir_all(root.join("vendor/lib")).expect("mkdir vendor");
    fs::write(
        root.join("vendor/lib/generated.rs"),
        "pub fn vendored() -> usize { 1 }\n",
    )
    .expect("write vendored");
    git_init_commit(&root);
    fs::write(
        root.join("vendor/lib/generated.rs"),
        "pub fn vendored() -> usize { 2 }\n",
    )
    .expect("modify vendored");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "diff".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({
                    "path": "vendor/lib/generated.rs",
                    "read_mode": "diff",
                    "diff_baseline": "worktree",
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["read_mode"], "diff");
    assert_eq!(
        result.content["ignored"], true,
        "diff mode must surface policy-ignored metadata: {}",
        result.content
    );
    assert_eq!(result.content["ignored_reason"], "vendor");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_slice_diff_last_receipt_ignores_line_number_gutter_for_unchanged_source() {
    // Bug #3: compaction stores the line-numbered `read_slice` render
    // (`"{line_no}\t{source}"`) as the snapshot content. The last-receipt diff
    // compared that gutter-prefixed text against the raw file bytes, so an
    // otherwise-unchanged file produced a spurious changed range on every line.
    // Store the line-numbered render of the *current* source and assert no
    // changed ranges survive.
    let root = temp_workspace("read_slice_last_receipt_line_gutter");
    let source = "alpha\nbeta\ngamma\n";
    fs::write(root.join("sample.txt"), source).expect("write sample");
    // Mirror exactly what compaction persists: the model-facing, line-numbered
    // render produced by `prefix_lines_with_numbers`.
    let line_numbered = crate::graph_tools::prefix_lines_with_numbers(source, 1);
    assert_ne!(
        line_numbered, source,
        "guard: stored content must actually carry the line-number gutter"
    );
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "sample.txt".to_string(),
            tool_name: "read_slice".to_string(),
            call_id: "prior_read".to_string(),
            stable_output_sha256: "prior-output".to_string(),
            // Deliberately stale hash so the unchanged-stub shortcut does not
            // fire and we exercise the byte-diff path that carried the bug.
            content_sha256: Some("stale-hash-forces-byte-diff".to_string()),
            start_byte: 0,
            end_byte: source.len() as u64,
            content: line_numbered,
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
                    "limit": source.len(),
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["baseline_used"], "last_receipt");
    let ranges = result.content["ranges"].as_array().expect("ranges");
    assert!(
        ranges.is_empty(),
        "line-numbered gutter must be stripped before diffing; spurious ranges: {ranges:?}"
    );

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
async fn read_slice_suppresses_resident_read_when_prior_window_encloses_unchanged_file() {
    // FIX B: a prior read whose byte window ENCLOSES this request, taken from an
    // unchanged file (SHA match), means the model already has these bytes in
    // context. Suppress the re-read with a receipt stub naming the prior call.
    let root = temp_workspace("read_slice_resident_dedup");
    let body = "alpha\nbeta\ngamma\n";
    fs::write(root.join("sample.txt"), body).expect("write sample");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "sample.txt".to_string(),
            tool_name: "read_slice".to_string(),
            call_id: "prior_full_read".to_string(),
            stable_output_sha256: "prior-output".to_string(),
            content_sha256: Some(sha256_hex(body.as_bytes())),
            // Encloses the requested [0, 5) window below.
            start_byte: 0,
            end_byte: body.len() as u64,
            content: body.to_string(),
            model_output_bytes: 256,
            created_unix_millis: 1,
        })
        .expect("put snapshot");
    let registry = registry_with_state_store(&root, store);

    let result = registry
        .execute(
            ToolCall {
                call_id: "slice".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({
                    "path": "sample.txt",
                    "offset": 0,
                    "limit": 5
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["receipt_stub"], true);
    assert_eq!(result.content["dedup"], true);
    assert_eq!(result.content["resident_read"], true);
    assert_eq!(result.content["unchanged"], true);
    assert_eq!(result.content["bytes_returned"], 0);
    assert_eq!(result.content["same_as_call_id"], "prior_full_read");
    assert_eq!(result.content["same_as_tool_name"], "read_slice");
    assert!(
        result.content.get("content").is_none(),
        "stub must not re-serialize content: {}",
        result.content
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_slice_does_not_suppress_when_prior_window_does_not_enclose_request() {
    // Recall safety: a prior read that only covers a SUBSET of the requested
    // window does not give the model the missing bytes, so the read must run.
    let root = temp_workspace("read_slice_resident_no_enclose");
    let body = "alpha\nbeta\ngamma\n";
    fs::write(root.join("sample.txt"), body).expect("write sample");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "sample.txt".to_string(),
            tool_name: "read_slice".to_string(),
            call_id: "prior_partial_read".to_string(),
            stable_output_sha256: "prior-output".to_string(),
            content_sha256: Some(sha256_hex(body.as_bytes())),
            // Only [0, 5) — does NOT enclose the requested [0, 16).
            start_byte: 0,
            end_byte: 5,
            content: "alpha".to_string(),
            model_output_bytes: 64,
            created_unix_millis: 1,
        })
        .expect("put snapshot");
    let registry = registry_with_state_store(&root, store);

    let result = registry
        .execute(
            ToolCall {
                call_id: "slice".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({
                    "path": "sample.txt",
                    "offset": 0,
                    "limit": 16
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert!(
        result.content.get("receipt_stub").is_none(),
        "non-enclosing prior read must not suppress the request: {}",
        result.content
    );
    assert!(
        result.content["content"].as_str().is_some(),
        "real read must return content: {}",
        result.content
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_slice_does_not_suppress_when_file_changed_since_prior_read() {
    // Recall safety: if the file changed since the prior read (SHA mismatch),
    // the resident bytes are stale, so the read must run.
    let root = temp_workspace("read_slice_resident_changed");
    fs::write(root.join("sample.txt"), "alpha\nbeta\ngamma\n").expect("write sample");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "sample.txt".to_string(),
            tool_name: "read_slice".to_string(),
            call_id: "prior_read".to_string(),
            stable_output_sha256: "prior-output".to_string(),
            // SHA of an older, different file body.
            content_sha256: Some(sha256_hex("OLD CONTENT".as_bytes())),
            start_byte: 0,
            end_byte: 64,
            content: "OLD CONTENT".to_string(),
            model_output_bytes: 64,
            created_unix_millis: 1,
        })
        .expect("put snapshot");
    let registry = registry_with_state_store(&root, store);

    let result = registry
        .execute(
            ToolCall {
                call_id: "slice".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({
                    "path": "sample.txt",
                    "offset": 0,
                    "limit": 5
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert!(
        result.content.get("receipt_stub").is_none(),
        "a changed file must not suppress the request: {}",
        result.content
    );
    assert!(result.content["content"].as_str().is_some());

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
    fs::create_dir_all(root.join("tools/sample-arch-graph/src")).expect("create crate");
    fs::write(
        root.join("tools/sample-arch-graph/Cargo.toml"),
        "[package]\nname = \"sample-arch-graph\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write manifest");

    let plan = verify_command_plan(
        &root,
        VerifyScope::Diff,
        VerifyLevel::Quick,
        &["tools/sample-arch-graph/src/main.rs".to_string()],
    )
    .expect("verification plan");

    assert_eq!(
        plan.command,
        "cargo test --manifest-path 'tools/sample-arch-graph/Cargo.toml' --message-format=json"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn cargo_setup_failure_reason_separates_environment_from_code_failures() {
    // The exact failure shape that keeps surfacing: a private git dependency
    // cargo can't fetch/authenticate, with a pinned revision that is gone.
    let dep_failure = "Updating git repository `https://github.com/SonarSource/semsitter.git`\n\
         error: failed to get `udg-gen` as a dependency of package `sonar-context-augmentation`\n\
         Caused by:\n  failed to load source for dependency `udg-gen`\n\
         Caused by:\n  revision 0a910a90 not found\n\
         Caused by:\n  failed to authenticate when downloading repository\n";
    assert!(cargo_setup_failure_reason(dep_failure).is_some());

    // A missing toolchain/std is also an environment failure.
    assert!(cargo_setup_failure_reason("can't find crate for `core`").is_some());

    // Genuine code-quality failures must NOT be masked as setup failures.
    assert!(
        cargo_setup_failure_reason(
            "error[E0425]: cannot find value `x`\n\
             error: could not compile `foo` (lib) due to 1 previous error"
        )
        .is_none()
    );
    assert!(
        cargo_setup_failure_reason(
            "test tests::it_works ... FAILED\nassertion `left == right` failed"
        )
        .is_none()
    );
    assert!(cargo_setup_failure_reason("").is_none());
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
async fn apply_patch_rejects_search_replace_then_move_on_same_path() {
    // A `search_replace` plus a `move_file` on the same source in one call is
    // staged against the original on-disk bytes, so the apply loop would write
    // the edited source, then move the *un-edited* original to the destination
    // and delete the source — silently losing the edit while reporting both
    // ops "applied". The call must be rejected and the file left untouched.
    let root = temp_workspace("apply_patch_conflict_replace_move");
    fs::write(root.join("a.rs"), "foo\n").expect("seed a.rs");
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_conflict".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "operations": [
                        {
                            "kind": "search_replace",
                            "path": "a.rs",
                            "search": "foo",
                            "replace": "bar",
                            "expected_sha256": sha256_hex("foo\n".as_bytes()),
                        },
                        {
                            "kind": "move_file",
                            "from": "a.rs",
                            "to": "b.rs",
                            "expected_sha256": sha256_hex("foo\n".as_bytes()),
                        }
                    ]
                }),
            },
            CancellationToken::new(),
            "turn-conflict".to_string(),
        )
        .await;

    assert_eq!(
        result.status,
        ToolStatus::Error,
        "overlapping cross-kind ops must be rejected, not silently clobbered: {:?}",
        result.content
    );
    assert_eq!(result.content["path"], "a.rs");
    assert_eq!(
        result.content["conflicting_kinds"],
        json!(["search_replace", "move_file"])
    );
    // Nothing was written: the source survives unchanged and the move
    // destination was never created.
    assert_eq!(fs::read_to_string(root.join("a.rs")).unwrap(), "foo\n");
    assert!(
        !root.join("b.rs").exists(),
        "move destination must not exist"
    );

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
    // The unified-diff fallback is squeezy's fuzz/whitespace-tolerant path:
    // when this lands the op the caller should be able to audit that it was
    // not an exact match.
    let delta = result
        .content
        .get("delta")
        .and_then(|v| v.as_array())
        .cloned()
        .expect("delta array");
    assert_eq!(delta.len(), 1);
    assert_eq!(delta[0]["status"], "applied");
    assert_eq!(delta[0]["exact"], false);
    assert_eq!(delta[0]["path"], "doc.txt");
    let applied_delta = result
        .content
        .get("applied_delta")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(applied_delta["exact"], false);
    assert_eq!(applied_delta["operations"][0]["exact"], false);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_recovers_from_curly_quote_drift() {
    // Audit F14-cc: the file uses a curly apostrophe (U+2019) but the model
    // emits ASCII `'` in both `search` and `replace`. The byte-exact lookup
    // misses, the quote-normalize fallback locates the slice, and the
    // replacement re-emits the curly apostrophe via `preserve_quote_style`
    // so the file's typography survives the edit. The applied_delta surfaces
    // the `fallback: "quote_normalize"` tag for the audit log.
    let root = temp_workspace("apply_patch_curly_quote");
    let initial = "We don\u{2019}t fly.\n";
    fs::write(root.join("doc.txt"), initial).expect("seed doc");
    let on_disk_hash = sha256_hex(initial.as_bytes());
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_curly".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "operations": [{
                        "kind": "search_replace",
                        "path": "doc.txt",
                        "search": "don't fly",
                        "replace": "don't run",
                        "expected_sha256": on_disk_hash,
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-curly".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    let final_doc = fs::read_to_string(root.join("doc.txt")).expect("read doc");
    assert_eq!(
        final_doc, "We don\u{2019}t run.\n",
        "curly apostrophe must survive the edit",
    );
    let applied_delta = result
        .content
        .get("applied_delta")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(applied_delta["exact"], false);
    assert_eq!(applied_delta["operations"][0]["exact"], false);
    assert_eq!(
        applied_delta["operations"][0]["fallback"],
        "quote_normalize"
    );
    let operations = result
        .content
        .get("operations")
        .and_then(|v| v.as_array())
        .cloned()
        .expect("operations preview");
    assert_eq!(operations[0]["fallback"], "quote_normalize");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_quote_fallback_counts_self_overlapping_match_once() {
    // Issue #274: the quote-normalize fallback counted matches with an
    // overlapping scan, so a self-overlapping search (`''`) against three
    // curly apostrophes was seen as two matches and rejected. The exact path
    // uses non-overlapping `match_indices` semantics; the fallback must agree.
    // Content `\u{2019}\u{2019}\u{2019}` normalizes to `'''`; `match_indices("''")`
    // on `'''` is exactly one, so this single edit must apply.
    let root = temp_workspace("apply_patch_quote_overlap");
    let initial = "\u{2019}\u{2019}\u{2019}\n";
    fs::write(root.join("doc.txt"), initial).expect("seed doc");
    let on_disk_hash = sha256_hex(initial.as_bytes());
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_quote_overlap".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "operations": [{
                        "kind": "search_replace",
                        "path": "doc.txt",
                        "search": "''",
                        "replace": "X",
                        "expected_sha256": on_disk_hash,
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-quote-overlap".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    let applied_delta = result
        .content
        .get("applied_delta")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        applied_delta["operations"][0]["fallback"],
        "quote_normalize"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_does_not_levenshtein_match() {
    // Audit F14-cc anti-pattern guard: the quote-normalize fallback must be
    // narrow. A 3-character-different search (no quote drift, just a typo)
    // must NOT be rescued — keep the fallback deterministic, no fuzzy creep.
    let root = temp_workspace("apply_patch_no_levenshtein");
    let initial = "hello world\n";
    fs::write(root.join("doc.txt"), initial).expect("seed doc");
    let on_disk_hash = sha256_hex(initial.as_bytes());
    let registry = registry_with_checkpoints(&root);

    // Three characters off ('helo wrld!' vs 'hello world') — no curly drift,
    // just a typo. Quote-normalize must NOT rescue this.
    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_fuzzy".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "operations": [{
                        "kind": "search_replace",
                        "path": "doc.txt",
                        "search": "helo wrld!",
                        "replace": "hi there",
                        "expected_sha256": on_disk_hash,
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-fuzzy".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Stale, "{:?}", result.content);
    assert_eq!(
        result.content["error"], "search text was not found",
        "no fuzzy match: must surface the exact error so the model re-reads"
    );
    let unchanged = fs::read_to_string(root.join("doc.txt")).expect("read doc");
    assert_eq!(unchanged, initial);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_recovers_from_nfkc_ligature_drift() {
    // F01: the file uses the NFKC-decomposable ligature `ﬁ` (U+FB01) but the
    // model emits ASCII `fi`. The byte-exact lookup misses, the quote-only
    // fallback also misses (no curly quotes), and the broader
    // unicode-normalize fallback locates the slice by running NFKC on both
    // sides. The applied_delta surfaces `fallback: "unicode_normalize"` for
    // the audit log so the operator can distinguish quote drift from the
    // wider unicode chain.
    let root = temp_workspace("apply_patch_nfkc_ligature");
    let initial = "let con\u{FB01}g = load();\n";
    fs::write(root.join("doc.txt"), initial).expect("seed doc");
    let on_disk_hash = sha256_hex(initial.as_bytes());
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_nfkc".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "operations": [{
                        "kind": "search_replace",
                        "path": "doc.txt",
                        "search": "let config = load();",
                        "replace": "let config = fetch();",
                        "expected_sha256": on_disk_hash,
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-nfkc".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    let final_doc = fs::read_to_string(root.join("doc.txt")).expect("read doc");
    assert_eq!(
        final_doc, "let config = fetch();\n",
        "NFKC fallback should replace the entire ligature-containing slice with the model's ASCII replacement",
    );
    let applied_delta = result
        .content
        .get("applied_delta")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(applied_delta["exact"], false);
    assert_eq!(applied_delta["operations"][0]["exact"], false);
    assert_eq!(
        applied_delta["operations"][0]["fallback"],
        "unicode_normalize"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_recovers_from_em_dash_drift() {
    // F01: the file has an em-dash (U+2014) but the model emitted ASCII
    // `--`. The unicode-normalize fallback collapses em-dashes to `--` on
    // both sides and finds the slice. The original em-dash is replaced by
    // the model's ASCII text verbatim; we do not try to re-emit the
    // typographic dash because the broader fallback intentionally tolerates
    // lossy unicode→ASCII edits.
    let root = temp_workspace("apply_patch_em_dash");
    let initial = "see chapter 3\u{2014}intro\n";
    fs::write(root.join("doc.txt"), initial).expect("seed doc");
    let on_disk_hash = sha256_hex(initial.as_bytes());
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_em_dash".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "operations": [{
                        "kind": "search_replace",
                        "path": "doc.txt",
                        "search": "chapter 3--intro",
                        "replace": "chapter 4 -- intro",
                        "expected_sha256": on_disk_hash,
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-em-dash".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    let final_doc = fs::read_to_string(root.join("doc.txt")).expect("read doc");
    assert_eq!(final_doc, "see chapter 4 -- intro\n");
    let applied_delta = result
        .content
        .get("applied_delta")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        applied_delta["operations"][0]["fallback"],
        "unicode_normalize"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_recovers_from_nbsp_drift() {
    // F01: the file has a non-breaking space (U+00A0) where the model
    // emitted a regular space. Common when prose was pasted from a Word
    // doc or a CMS that auto-inserts NBSP between a number and a unit. The
    // unicode-normalize fallback should bridge that gap.
    let root = temp_workspace("apply_patch_nbsp");
    let initial = "size\u{00A0}10 MB\n";
    fs::write(root.join("doc.txt"), initial).expect("seed doc");
    let on_disk_hash = sha256_hex(initial.as_bytes());
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_nbsp".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "operations": [{
                        "kind": "search_replace",
                        "path": "doc.txt",
                        "search": "size 10 MB",
                        "replace": "size 20 MB",
                        "expected_sha256": on_disk_hash,
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-nbsp".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    let final_doc = fs::read_to_string(root.join("doc.txt")).expect("read doc");
    assert_eq!(final_doc, "size 20 MB\n");
    let applied_delta = result
        .content
        .get("applied_delta")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        applied_delta["operations"][0]["fallback"],
        "unicode_normalize"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_batched_edits_same_file_apply_in_order() {
    // F01: the batched form (multiple operations targeting the same file)
    // must apply edits in array order — each subsequent search_replace runs
    // against the staged state produced by the previous edit, not the
    // pristine file. Cheap to verify by chaining two transformations that
    // are only reachable when the first one already landed.
    let root = temp_workspace("apply_patch_batch_order");
    let initial = "alpha\nbeta\ngamma\n";
    fs::write(root.join("doc.txt"), initial).expect("seed doc");
    let on_disk_hash = sha256_hex(initial.as_bytes());
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_batch_order".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "operations": [
                        {
                            "kind": "search_replace",
                            "path": "doc.txt",
                            "search": "alpha\n",
                            "replace": "ALPHA-step1\n",
                            "expected_sha256": on_disk_hash,
                        },
                        {
                            "kind": "search_replace",
                            "path": "doc.txt",
                            "search": "ALPHA-step1\n",
                            "replace": "ALPHA-step2\n",
                            "expected_sha256": on_disk_hash,
                        }
                    ]
                }),
            },
            CancellationToken::new(),
            "turn-batch-order".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    let final_doc = fs::read_to_string(root.join("doc.txt")).expect("read doc");
    assert_eq!(
        final_doc, "ALPHA-step2\nbeta\ngamma\n",
        "second edit must see the first edit's output, not the pristine file",
    );
    let delta = result
        .content
        .get("delta")
        .and_then(|v| v.as_array())
        .cloned()
        .expect("top-level delta array");
    assert_eq!(delta.len(), 2);
    assert_eq!(delta[0]["status"], "applied");
    assert_eq!(delta[1]["status"], "applied");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_batched_edits_fail_atomically_on_missing_search() {
    // F01: when one edit in a batch fails because its search text is not
    // found, the whole batch must be rejected and earlier edits must not
    // touch disk. The error must point at the offending index so the model
    // can re-emit the right slice without re-deriving which patch was bad.
    let root = temp_workspace("apply_patch_batch_fail");
    let initial = "alpha\nbeta\ngamma\n";
    fs::write(root.join("doc.txt"), initial).expect("seed doc");
    let on_disk_hash = sha256_hex(initial.as_bytes());
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_batch_fail".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "operations": [
                        {
                            "kind": "search_replace",
                            "path": "doc.txt",
                            "search": "alpha\n",
                            "replace": "ALPHA\n",
                            "expected_sha256": on_disk_hash,
                        },
                        {
                            "kind": "search_replace",
                            "path": "doc.txt",
                            "search": "this-text-is-not-in-the-file\n",
                            "replace": "anything\n",
                            "expected_sha256": on_disk_hash,
                        }
                    ]
                }),
            },
            CancellationToken::new(),
            "turn-batch-fail".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Stale, "{:?}", result.content);
    assert_eq!(result.content["error"], "search text was not found");
    assert_eq!(
        result.content["patch_index"], 1,
        "error must point at the offending batch index",
    );
    let unchanged = fs::read_to_string(root.join("doc.txt")).expect("read doc");
    assert_eq!(
        unchanged, initial,
        "batched edits must roll back atomically — first edit's staged write must not hit disk when a later edit fails",
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_returns_per_op_delta_with_exact_flag_on_success() {
    // Audit F14: a multi-op apply_patch success must expose a per-op `delta`
    // array — one entry per requested op, each `applied` and `exact=true`
    // when the search-replace matched the pre-image byte-for-byte.
    let root = temp_workspace("apply_patch_delta_exact");
    fs::write(root.join("alpha.txt"), "alpha-before\n").expect("write alpha");
    fs::write(root.join("beta.txt"), "beta-before\n").expect("write beta");
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_delta_exact".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "patches": [
                        {
                            "path": "alpha.txt",
                            "search": "alpha-before\n",
                            "replace": "alpha-after\n",
                            "expected_sha256": sha256_hex("alpha-before\n".as_bytes()),
                        },
                        {
                            "path": "beta.txt",
                            "search": "beta-before\n",
                            "replace": "beta-after\n",
                            "expected_sha256": sha256_hex("beta-before\n".as_bytes()),
                        }
                    ]
                }),
            },
            CancellationToken::new(),
            "turn-delta-exact".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    let delta = result
        .content
        .get("delta")
        .and_then(|v| v.as_array())
        .cloned()
        .expect("top-level delta array");
    assert_eq!(delta.len(), 2, "one entry per op: {delta:?}");
    for (idx, entry) in delta.iter().enumerate() {
        assert_eq!(
            entry["status"], "applied",
            "op {idx} status mismatch: {entry}"
        );
        assert_eq!(
            entry["exact"], true,
            "op {idx} should be an exact match: {entry}"
        );
        assert!(
            entry.get("error").is_none(),
            "exact success must not carry an error field: {entry}"
        );
    }
    assert_eq!(delta[0]["path"], "alpha.txt");
    assert_eq!(delta[1]["path"], "beta.txt");
    // applied_delta.operations should also carry the per-op exact flag.
    let applied_delta = result.content.get("applied_delta").expect("applied_delta");
    let ops = applied_delta
        .get("operations")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0]["exact"], true);
    assert_eq!(ops[1]["exact"], true);
    assert_eq!(applied_delta["exact"], true);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_returns_unified_diff_with_hunk_header_and_markers() {
    // F14 (unified_diff): a successful apply_patch must surface a
    // `unified_diff` string the caller can pipe through `git apply` to
    // reconstruct the edit. For a 2-line search/replace the diff must carry
    // the standard `--- a/<path>` / `+++ b/<path>` headers, a `@@` hunk
    // header, and matched `-`/`+` line markers around the change.
    let root = temp_workspace("apply_patch_unified_diff_output");
    let before = "line-one\nline-two\nline-three\n";
    fs::write(root.join("sample.txt"), before).expect("write sample");
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_unified_diff".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "patches": [{
                        "path": "sample.txt",
                        "search": "line-one\nline-two\n",
                        "replace": "line-one-new\nline-two-new\n",
                        "expected_sha256": sha256_hex(before.as_bytes()),
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-unified-diff".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    let unified = result
        .content
        .get("unified_diff")
        .and_then(Value::as_str)
        .expect("unified_diff string");
    assert!(
        unified.contains("--- a/sample.txt"),
        "missing old-file header: {unified}"
    );
    assert!(
        unified.contains("+++ b/sample.txt"),
        "missing new-file header: {unified}"
    );
    assert!(unified.contains("@@"), "missing hunk header: {unified}");
    assert!(
        unified.contains("-line-one\n"),
        "missing `-` marker for removed line: {unified}"
    );
    assert!(
        unified.contains("+line-one-new\n"),
        "missing `+` marker for added line: {unified}"
    );
    assert!(
        unified.contains("-line-two\n") && unified.contains("+line-two-new\n"),
        "second changed line should appear as -/+: {unified}"
    );

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn apply_patch_delta_reports_per_op_failure_with_error() {
    // Audit F14: when an op fails mid-apply, the per-op delta must mark that
    // op `failed` with an `error` field populated, and any later ops must be
    // `skipped` — the caller needs enough information to audit which path is
    // broken without reparsing the top-level error string.
    use std::os::unix::fs::PermissionsExt;

    let root = temp_workspace("apply_patch_delta_failure");
    fs::write(root.join("first.txt"), "first-before\n").expect("write first");
    fs::write(root.join("second.txt"), "second-before\n").expect("write second");
    fs::write(root.join("third.txt"), "third-before\n").expect("write third");
    let read_only = root.join("second.txt");
    let mut perms = fs::metadata(&read_only).expect("read meta").permissions();
    perms.set_mode(0o444);
    fs::set_permissions(&read_only, perms).expect("set readonly");
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_delta_failure".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "patches": [
                        {
                            "path": "first.txt",
                            "search": "first-before\n",
                            "replace": "first-after\n",
                            "expected_sha256": sha256_hex("first-before\n".as_bytes()),
                        },
                        {
                            "path": "second.txt",
                            "search": "second-before\n",
                            "replace": "second-after\n",
                            "expected_sha256": sha256_hex("second-before\n".as_bytes()),
                        },
                        {
                            "path": "third.txt",
                            "search": "third-before\n",
                            "replace": "third-after\n",
                            "expected_sha256": sha256_hex("third-before\n".as_bytes()),
                        }
                    ]
                }),
            },
            CancellationToken::new(),
            "turn-delta-failure".to_string(),
        )
        .await;

    if let Ok(meta) = fs::metadata(&read_only) {
        let mut perms = meta.permissions();
        perms.set_mode(0o644);
        let _ = fs::set_permissions(&read_only, perms);
    }

    if result.status == ToolStatus::Error {
        let delta = result
            .content
            .get("delta")
            .and_then(|v| v.as_array())
            .cloned()
            .expect("delta array on partial failure");
        assert_eq!(delta.len(), 3, "one entry per op: {delta:?}");
        assert_eq!(delta[0]["status"], "applied");
        assert_eq!(delta[0]["exact"], true);
        assert!(delta[0].get("error").is_none());

        assert_eq!(delta[1]["status"], "failed");
        assert_eq!(delta[1]["exact"], true);
        assert!(
            delta[1]["error"].as_str().is_some_and(|s| !s.is_empty()),
            "failed op must surface its error string: {}",
            delta[1]
        );
        assert_eq!(delta[1]["path"], "second.txt");

        assert_eq!(
            delta[2]["status"], "skipped",
            "ops after the failure must be skipped: {}",
            delta[2]
        );
        assert_eq!(delta[2]["exact"], true);
    } else {
        // Some sandboxes (root, etc) ignore 0o444; in that case every op
        // applied and the failure-path assertions don't fire.
        assert_eq!(result.status, ToolStatus::Success);
    }

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_without_expected_sha256_uses_server_side_seen_state() {
    // F14-cc-server-side-read-state-gate: when the model omits
    // `expected_sha256` and a prior `read_file` snapshot for the same path
    // still matches the on-disk hash, apply_patch should proceed exactly as
    // if the model had threaded the hash back through the patch arguments.
    let root = temp_workspace("apply_patch_seen_state_ok");
    let body = "before\n";
    fs::write(root.join("sample.txt"), body).expect("write sample");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "sample.txt".to_string(),
            tool_name: "read_file".to_string(),
            call_id: "first_read".to_string(),
            stable_output_sha256: "output-hash".to_string(),
            content_sha256: Some(sha256_hex(body.as_bytes())),
            start_byte: 0,
            end_byte: body.len() as u64,
            content: body.to_string(),
            model_output_bytes: 64,
            created_unix_millis: 1,
        })
        .expect("put snapshot");
    let registry = registry_with_state_store_and_checkpoints(&root, store);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_no_hash".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "patches": [{
                        "path": "sample.txt",
                        "search": "before\n",
                        "replace": "after\n",
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-seen-state".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    assert_eq!(
        fs::read_to_string(root.join("sample.txt")).unwrap(),
        "after\n"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_without_seen_state_demands_read_first() {
    // F14-cc-server-side-read-state-gate: when the model never read the file
    // and omits `expected_sha256`, the server cannot vouch for the model's
    // view of the file. apply_patch must refuse with a hint that points at
    // `read_file` instead of writing blind.
    let root = temp_workspace("apply_patch_seen_state_missing");
    fs::write(root.join("sample.txt"), "before\n").expect("write sample");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    let registry = registry_with_state_store_and_checkpoints(&root, store);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_no_snapshot".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "patches": [{
                        "path": "sample.txt",
                        "search": "before\n",
                        "replace": "after\n",
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-no-snapshot".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Stale);
    let error = result.content["error"].as_str().unwrap_or("");
    assert!(
        error.contains("call read_file first"),
        "expected read_file hint, got: {error}"
    );
    // File must be untouched.
    assert_eq!(
        fs::read_to_string(root.join("sample.txt")).unwrap(),
        "before\n"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_without_expected_sha256_detects_external_drift() {
    // F14-cc-server-side-read-state-gate: when the model has a stale read
    // snapshot (the file changed on disk between read_file and apply_patch),
    // the server-side gate must catch the drift and refuse, naming both the
    // last-seen and current hashes so the model can recover.
    let root = temp_workspace("apply_patch_seen_state_drift");
    let prior = "before\n";
    let current = "before-modified-externally\n";
    fs::write(root.join("sample.txt"), current).expect("write sample");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    let prior_sha = sha256_hex(prior.as_bytes());
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "sample.txt".to_string(),
            tool_name: "read_file".to_string(),
            call_id: "stale_read".to_string(),
            stable_output_sha256: "output-stale".to_string(),
            content_sha256: Some(prior_sha.clone()),
            start_byte: 0,
            end_byte: prior.len() as u64,
            content: prior.to_string(),
            model_output_bytes: 64,
            created_unix_millis: 1,
        })
        .expect("put snapshot");
    let registry = registry_with_state_store_and_checkpoints(&root, store);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_drift".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "patches": [{
                        "path": "sample.txt",
                        "search": "before-modified-externally\n",
                        "replace": "after\n",
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-drift".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Stale);
    let error = result.content["error"].as_str().unwrap_or("");
    assert!(
        error.contains("file changed since the model last saw it"),
        "expected drift error, got: {error}"
    );
    assert_eq!(result.content["last_seen_sha256"], prior_sha);
    assert_eq!(result.content["last_read_call_id"], "stale_read");
    assert_eq!(
        result.content["current_sha256"],
        sha256_hex(current.as_bytes())
    );
    // File must be untouched.
    assert_eq!(
        fs::read_to_string(root.join("sample.txt")).unwrap(),
        current
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
async fn plan_patch_candidate_paths_are_included_in_neighborhood() {
    let root = temp_workspace("plan_binding_candidate");
    write_rust_crate(
        &root,
        "pub fn target() -> usize { 1 }\nfn caller() -> usize { target() }\n",
    );
    fs::write(root.join("stranger.txt"), "out\n").expect("write stranger");
    let registry = registry_with_checkpoints(&root);

    let plan = registry
        .execute(
            ToolCall {
                call_id: "plan_candidate".to_string(),
                name: "plan_patch".to_string(),
                arguments: json!({
                    "objective": "tweak target",
                    "query": "target",
                    "kind": "function",
                    "candidate_paths": ["stranger.txt"],
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(plan.status, ToolStatus::Success);
    if plan
        .content
        .get("graph_available")
        .and_then(|v| v.as_bool())
        == Some(false)
    {
        let _ = fs::remove_dir_all(root);
        return;
    }
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
                call_id: "apply_candidate".to_string(),
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
            "turn-plan-candidate".to_string(),
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
    assert_eq!(plain.content["content"], "1\tvisible");

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
            .contains("\"bytes_returned\":30000")
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
async fn spill_envelope_includes_recovery_hint_and_on_disk_path() {
    // squeezy-uq1g: when a tool result overflows the spill threshold, the
    // envelope handed back to the model must name the recovery tool, the
    // recovery arguments, the on-disk path, and carry a short human-
    // readable hint. Without these the model has to infer recovery from
    // its tool registry and the TUI has no way to surface a tail command.
    let root = temp_workspace("spill_recovery_envelope");
    fs::write(root.join("payload.txt"), "y".repeat(2_048)).expect("write payload");
    let registry = ToolRegistry::new_with_output_config(
        &root,
        ToolOutputConfig {
            spill_threshold_bytes: 128,
            preview_bytes: 32,
            retention_days: 1,
            output_dir: None,
        },
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_envelope".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "payload.txt", "limit": 10_000}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let content = &result.content;
    assert_eq!(content["spilled"], true);
    let handle = content["handle"].as_str().expect("handle");

    // Recovery shape: tool name + args mirroring the handle.
    assert_eq!(content["recovery_tool"], "read_tool_output");
    assert_eq!(content["recovery_args"]["handle"], handle);

    // on_disk_path must point at the spilled file under the workspace.
    // The producer joins paths against the un-canonicalized workspace
    // root, so strip the `\\?\` Windows extended-length prefix that
    // `fs::canonicalize` adds before comparing — otherwise the test
    // fails on Windows even though both sides reference the same file.
    let on_disk_path = content["on_disk_path"]
        .as_str()
        .expect("on_disk_path string");
    let expected_path = strip_verbatim_prefix(root.canonicalize().expect("canonical root"))
        .join(".squeezy")
        .join("tool_outputs")
        .join(format!("{handle}.json"));
    assert_eq!(
        PathBuf::from(on_disk_path),
        expected_path,
        "on_disk_path must match the file on disk"
    );
    assert!(
        PathBuf::from(on_disk_path).is_file(),
        "on_disk_path must exist: {on_disk_path}"
    );

    // recovery_hint must mention the tool, the handle, and the path so a
    // human reading the raw envelope (or a TUI mirror) can act on it.
    let hint = content["recovery_hint"].as_str().expect("recovery_hint");
    assert!(
        hint.contains("read_tool_output"),
        "hint must name the tool: {hint}"
    );
    assert!(
        hint.contains(handle),
        "hint must reference the handle: {hint}"
    );

    // Snapshot the envelope keys so accidental drops or renames trip the
    // test instead of silently regressing wave-2 finding squeezy-uq1g.
    let mut keys = content
        .as_object()
        .expect("envelope is an object")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "handle".to_string(),
            "on_disk_path".to_string(),
            "original_output_sha256".to_string(),
            "preview".to_string(),
            "preview_bytes".to_string(),
            "recovery_args".to_string(),
            "recovery_hint".to_string(),
            "recovery_tool".to_string(),
            "sha256".to_string(),
            "spilled".to_string(),
            "total_bytes".to_string(),
            "truncated".to_string(),
        ],
        "spill envelope keys drifted; update squeezy-uq1g snapshot deliberately"
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_file_concurrent_distinct_paths_run_in_parallel() {
    // F01: two `write_file` calls against distinct files must not
    // serialise. We hold the per-realpath lock for path B externally,
    // which would block any `write_file` keyed on B; the call against
    // path A must still complete promptly because the locks are keyed
    // per-realpath rather than process-wide.
    let root = temp_workspace("write_file_concurrent_distinct");
    fs::write(root.join("a.txt"), b"a-before").expect("write a");
    fs::write(root.join("b.txt"), b"b-before").expect("write b");
    let registry = ToolRegistry::new(&root).expect("registry");

    let blocked_path = root.join("b.txt");
    let parked_guard = file_mutation_queue::lock_paths_for_mutation([&blocked_path]).await;

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        registry
            .execute(
                ToolCall {
                    call_id: "write_a".to_string(),
                    name: "write_file".to_string(),
                    arguments: json!({
                        "path": "a.txt",
                        "content": "a-after",
                        "expected_sha256": sha256_hex(b"a-before"),
                    }),
                },
                CancellationToken::new(),
            )
            .await
    })
    .await
    .expect("write_file against a.txt must not be blocked by an unrelated lock on b.txt");

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(
        fs::read_to_string(root.join("a.txt")).unwrap(),
        "a-after",
        "a.txt should have been written while b.txt's lock was held"
    );
    assert_eq!(
        fs::read_to_string(root.join("b.txt")).unwrap(),
        "b-before",
        "b.txt must remain untouched because no writer ran against it"
    );

    drop(parked_guard);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_file_concurrent_same_path_serialises_on_realpath() {
    // F01: a `write_file` against path P must wait if the per-realpath
    // lock for P is already held. We park the lock externally, kick off
    // the writer, observe that it does not complete, then release the
    // lock and confirm it then completes.
    let root = temp_workspace("write_file_concurrent_same");
    fs::write(root.join("shared.txt"), b"v0").expect("write shared");
    let registry = ToolRegistry::new(&root).expect("registry");

    let blocked_path = root.join("shared.txt");
    let parked_guard = file_mutation_queue::lock_paths_for_mutation([&blocked_path]).await;

    let registry_clone = registry.clone();
    let mut writer = tokio::spawn(async move {
        registry_clone
            .execute(
                ToolCall {
                    call_id: "write_shared".to_string(),
                    name: "write_file".to_string(),
                    arguments: json!({
                        "path": "shared.txt",
                        "content": "v1",
                        "expected_sha256": sha256_hex(b"v0"),
                    }),
                },
                CancellationToken::new(),
            )
            .await
    });

    // The writer must still be blocked on the per-realpath lock.
    let race = tokio::time::timeout(std::time::Duration::from_millis(150), &mut writer).await;
    assert!(
        race.is_err(),
        "write_file should block while the per-realpath lock is held"
    );
    assert_eq!(
        fs::read_to_string(root.join("shared.txt")).unwrap(),
        "v0",
        "file must not have been overwritten while the lock was held"
    );

    drop(parked_guard);
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), writer)
        .await
        .expect("writer must complete once the lock is released")
        .expect("writer task join");
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(fs::read_to_string(root.join("shared.txt")).unwrap(), "v1");

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
async fn checkpoint_provider_default_journal_still_records_through_trait() {
    // F14: when `checkpoints_enabled` is set, the registry must
    // auto-install the journal-backed CheckpointProvider, and edits must
    // still flow through it. The shape of the `checkpoint` field on the
    // tool result is the contract external tooling (TUI, undo flow)
    // depends on, so we assert both that the bridge fired and that the
    // record is in the legacy CRUD surface.
    let root = temp_workspace("checkpoint_provider_default_journal");
    fs::write(root.join("sample.txt"), "before").expect("write sample");
    let registry = registry_with_checkpoints(&root);

    assert!(
        registry.has_checkpoint_provider(),
        "registry_with_checkpoints must auto-install the journal provider",
    );

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
            "turn-journal".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let checkpoint = result
        .content
        .get("checkpoint")
        .expect("default provider must attach a checkpoint field");
    assert_eq!(checkpoint["tool_name"], "write_file");
    assert_eq!(checkpoint["group_id"], "turn-journal");
    assert!(
        checkpoint["files"]
            .as_array()
            .is_some_and(|files| !files.is_empty()),
        "journal record should list the mutated file: {checkpoint}",
    );

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
    let listed = list.content["checkpoints"]
        .as_array()
        .expect("checkpoints array");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["tool_name"], "write_file");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn checkpoint_provider_mock_observes_before_and_after_callbacks() {
    // F14: an external impl must be able to replace the journal-backed
    // provider on a stock registry and observe both halves of the bridge.
    // We construct a registry with checkpoints disabled (so no journal
    // provider is auto-installed), register a counting mock, and confirm
    // a single write_file call drives one before_edit / one after_edit
    // pair with the expected context and that the mock's JSON value is
    // surfaced under `checkpoint`.
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    #[derive(Default)]
    struct CountingProvider {
        before_calls: AtomicUsize,
        after_calls: AtomicUsize,
        last_tool: Mutex<Option<String>>,
        last_group: Mutex<Option<String>>,
        last_call_id: Mutex<Option<String>>,
        last_status: Mutex<Option<String>>,
    }

    impl CheckpointProvider for CountingProvider {
        fn before_edit(&self) -> squeezy_core::Result<Option<CheckpointSnapshot>> {
            self.before_calls.fetch_add(1, AtomicOrdering::SeqCst);
            // Encode a sentinel string in the opaque snapshot so the
            // matching after_edit can prove the registry round-tripped
            // the exact value back to us.
            Ok(Some(CheckpointSnapshot::new(
                "mock-sentinel-v1".to_string(),
            )))
        }

        fn after_edit(
            &self,
            before: &CheckpointSnapshot,
            context: &CheckpointEditContext,
        ) -> squeezy_core::Result<Option<Value>> {
            self.after_calls.fetch_add(1, AtomicOrdering::SeqCst);
            *self.last_tool.lock().unwrap() = Some(context.tool_name.clone());
            *self.last_group.lock().unwrap() = Some(context.group_id.clone());
            *self.last_call_id.lock().unwrap() = Some(context.call_id.clone());
            *self.last_status.lock().unwrap() = Some(context.status.to_string());
            let sentinel = before
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            Ok(Some(json!({
                "mock": true,
                "sentinel": sentinel,
                "tool_name": context.tool_name,
                "group_id": context.group_id,
            })))
        }
    }

    let root = temp_workspace("checkpoint_provider_mock");
    fs::write(root.join("sample.txt"), "before").expect("write sample");
    // Default-config registry has no journal: the bridge must still
    // accept an externally-registered provider so a git-stash-style
    // extension can plug in without forking core.
    let registry = registry_with_shell_sandbox_off(&root);
    assert!(
        !registry.has_checkpoint_provider(),
        "default registry without checkpoints must start with no provider",
    );

    let mock = Arc::new(CountingProvider::default());
    let previous =
        registry.register_checkpoint_provider(mock.clone() as Arc<dyn CheckpointProvider>);
    assert!(
        previous.is_none(),
        "no provider was installed before this test"
    );
    assert!(registry.has_checkpoint_provider());

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "write-mock".to_string(),
                name: "write_file".to_string(),
                arguments: json!({
                    "path": "sample.txt",
                    "content": "after",
                    "expected_sha256": sha256_hex("before".as_bytes()),
                }),
            },
            CancellationToken::new(),
            "turn-mock".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(
        mock.before_calls.load(AtomicOrdering::SeqCst),
        1,
        "before_edit must fire exactly once per edit-bearing tool call",
    );
    assert_eq!(
        mock.after_calls.load(AtomicOrdering::SeqCst),
        1,
        "after_edit must fire exactly once per edit-bearing tool call",
    );
    assert_eq!(
        mock.last_tool.lock().unwrap().as_deref(),
        Some("write_file"),
    );
    assert_eq!(
        mock.last_group.lock().unwrap().as_deref(),
        Some("turn-mock")
    );
    assert_eq!(
        mock.last_call_id.lock().unwrap().as_deref(),
        Some("write-mock"),
    );
    assert_eq!(mock.last_status.lock().unwrap().as_deref(), Some("success"));

    let checkpoint = result
        .content
        .get("checkpoint")
        .expect("mock-provided checkpoint must be attached to the result");
    assert_eq!(checkpoint["mock"], true);
    assert_eq!(checkpoint["sentinel"], "mock-sentinel-v1");
    assert_eq!(checkpoint["tool_name"], "write_file");
    assert_eq!(checkpoint["group_id"], "turn-mock");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn write_file_identical_content_is_noop_and_preserves_mtime() {
    // F14: writing the same bytes that are already on disk must
    // short-circuit. The tool result signals `noop=true`, no `fs::write`
    // occurs (verified by mtime preservation), and the checkpoint layer
    // does not record a change because the worktree tree is unchanged.
    let root = temp_workspace("write_file_noop_identical");
    let target = root.join("sample.txt");
    fs::write(&target, "same").expect("write sample");
    let mtime_before = fs::metadata(&target)
        .expect("metadata")
        .modified()
        .expect("mtime supported");

    // Push past the filesystem's mtime resolution so a real write would
    // land on a strictly later instant. Modern macOS/Linux give sub-ms
    // precision; the small sleep absorbs the rare low-resolution case
    // without bloating overall test runtime.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let registry = registry_with_checkpoints(&root);
    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "noop-write".to_string(),
                name: "write_file".to_string(),
                arguments: json!({
                    "path": "sample.txt",
                    "content": "same",
                    "expected_sha256": sha256_hex(b"same"),
                }),
            },
            CancellationToken::new(),
            "turn-noop".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["noop"], json!(true));
    assert_eq!(result.content["bytes_written"], json!(0));
    let expected_sha = sha256_hex(b"same");
    assert_eq!(result.content["before_sha256"], json!(&expected_sha));
    assert_eq!(result.content["after_sha256"], json!(&expected_sha));
    assert!(
        result.content.get("checkpoint").is_none(),
        "noop write must not emit a checkpoint, got {:?}",
        result.content.get("checkpoint")
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "same");

    let mtime_after = fs::metadata(&target)
        .expect("metadata")
        .modified()
        .expect("mtime supported");
    assert_eq!(
        mtime_before, mtime_after,
        "noop write must not touch the file (mtime should be unchanged)"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn write_file_changed_content_marks_noop_false_and_writes() {
    // F14 counterpart: a real edit must still proceed and the tool
    // result must explicitly mark `noop=false` so downstream layers know
    // a change really did land on disk.
    let root = temp_workspace("write_file_noop_changed");
    let target = root.join("sample.txt");
    fs::write(&target, "before").expect("write sample");
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "real-write".to_string(),
                name: "write_file".to_string(),
                arguments: json!({
                    "path": "sample.txt",
                    "content": "after",
                    "expected_sha256": sha256_hex(b"before"),
                }),
            },
            CancellationToken::new(),
            "turn-real".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["noop"], json!(false));
    assert_eq!(result.content["bytes_written"], json!("after".len()));
    assert_eq!(
        result.content["before_sha256"],
        json!(sha256_hex(b"before"))
    );
    assert_eq!(result.content["after_sha256"], json!(sha256_hex(b"after")));
    assert!(
        result.content["checkpoint"]["group_id"].is_string(),
        "real edit must emit a checkpoint, got {:?}",
        result.content.get("checkpoint")
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "after");

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn write_file_replaces_atomically_and_preserves_mode() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    // A replace must not truncate-in-place: write_file routes through a
    // sibling tempfile + rename so a crash leaves the original intact. The
    // observable signatures are that the file's mode survives the edit and
    // that the rename swaps in a fresh inode (an in-place `fs::write` would
    // keep the same inode and could leave a half-written file on crash).
    let root = temp_workspace("write_file_atomic_mode");
    let target = root.join("script.sh");
    fs::write(&target, "before").expect("write sample");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).expect("chmod");
    let ino_before = fs::metadata(&target).unwrap().ino();
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "atomic-write".to_string(),
                name: "write_file".to_string(),
                arguments: json!({
                    "path": "script.sh",
                    "content": "after",
                    "expected_sha256": sha256_hex(b"before"),
                }),
            },
            CancellationToken::new(),
            "turn-atomic".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(fs::read_to_string(&target).unwrap(), "after");

    let meta = fs::metadata(&target).unwrap();
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o755,
        "replace must preserve the original file mode"
    );
    assert_ne!(
        ino_before,
        meta.ino(),
        "atomic replace must swap a fresh inode in via rename, not truncate in place"
    );

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
                    "direct_user_shell": true,
                    "direct_user_shell_nonce": crate::direct_user_shell_nonce(),
                }),
            },
            CancellationToken::new(),
            "turn-direct".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    // Direct user-shell fast path skips checkpointing — the absence of a
    // checkpoint receipt is the distinguishing signal in a checkpoint-enabled
    // registry (the non-direct path in the same registry always writes one).
    assert!(result.content.get("checkpoint").is_none());
    assert_eq!(
        fs::read_to_string(root.join("direct.txt")).unwrap(),
        "direct"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn direct_user_shell_rejects_wrong_nonce() {
    // A caller that knows the `local-shell-` call_id convention but ships
    // a forged (or missing) nonce must NOT be allowed to skip the sandbox
    // / checkpoint guarantees. The bypass is gated on the process-local
    // secret, so only the in-process TUI minter can ever satisfy both
    // halves of the check.
    let root = temp_workspace("direct_user_shell_wrong_nonce");
    let registry = registry_with_shell_sandbox_off_and_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "local-shell-spoof".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf forged > forged.txt",
                    "description": "attempt to spoof the user-shell fast path",
                    "direct_user_shell": true,
                    "direct_user_shell_nonce": "not-the-real-nonce",
                }),
            },
            CancellationToken::new(),
            "turn-spoof".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    // Bypass refused: the call falls through to the normal model path, so a
    // checkpoint gets created for the destructive command — the very
    // protection the bypass would have skipped.
    assert_eq!(result.content["checkpoint"]["group_id"], "turn-spoof");

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
    let audit = fs::read_to_string(root.join(".squeezy/audit/shell.jsonl")).expect("audit log");
    assert!(audit.contains("\"capability\":\"search\""));
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

    let audit = fs::read_to_string(root.join(".squeezy/audit/shell.jsonl")).expect("audit log");
    assert!(audit.contains("\"capability\":\"git\""));
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
async fn checkpoint_undo_on_empty_store_returns_calm_nothing_to_undo_message() {
    // `/undo` against a fresh checkpoint store is a clean-tree no-op:
    // nothing has been recorded, so there is nothing to roll back. The
    // tools layer must distinguish that case from Stale (partial /
    // conflict) and Error, returning Success with a structured `message`
    // so downstream chrome can render the calm informational card.
    let root = temp_workspace("checkpoint_undo_empty_store");
    let registry = registry_with_checkpoints(&root);

    let undo = registry
        .execute(
            ToolCall {
                call_id: "undo-empty".to_string(),
                name: "checkpoint_undo".to_string(),
                arguments: json!({}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(
        undo.status,
        ToolStatus::Success,
        "empty checkpoint store is a happy-path no-op, not a failure",
    );
    assert_eq!(undo.content["message"], "nothing to undo");
    assert_eq!(undo.content["rollback"]["skipped"], true);
    assert_eq!(undo.content["rollback"]["applied"], false);
    assert!(
        undo.content["rollback"]["conflicts"]
            .as_array()
            .map(|c| c.is_empty())
            .unwrap_or(false),
        "skipped rollback should carry no conflicts",
    );
    assert!(
        undo.content.get("error").is_none() && undo.content.get("reason").is_none(),
        "calm informational result must not carry error/reason fields",
    );

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

    let _ = std::fs::remove_file(std::env::temp_dir().join("squeezy-inline-warning-test"));
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
    let audit = fs::read_to_string(root.join(".squeezy/audit/shell.jsonl")).expect("audit log");
    assert!(audit.contains("\"call_id\":\"call_1\""));
    assert!(audit.contains("\"stdout_sha256\""));
    assert!(!audit.contains("\"stdout\":\"abc\""));
    assert!(audit.contains("\"policy\":\"allowlist\""));
    assert!(audit.contains("\"mode\":\"off\""));
    assert!(audit.contains("\"parser_backed\":true"));

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
    let audit = fs::read_to_string(root.join(".squeezy/audit/shell.jsonl")).expect("audit log");
    assert!(audit.contains("\"mode\":\"best_effort\""));

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
    let audit = fs::read_to_string(root.join(".squeezy/audit/shell.jsonl")).expect("audit log");
    assert!(audit.contains("\"backend\":\"external\""));
    assert!(audit.contains("\"mode\":\"external\""));
    assert!(audit.contains("\"network\":\"external\""));
    assert!(audit.contains("\"policy\":\"allowlist\""));

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

#[test]
fn shell_segment_writes_filesystem_covers_metadata_write_verbs() {
    use crate::shell::shell_segment_writes_filesystem;

    for verb_segment in [
        "mkdir .git/hooks",
        "chmod 600 .git/config",
        "ln -s /tmp/x .git/HEAD",
        "mv tmp .git/config",
        "touch .git/index",
    ] {
        assert!(
            shell_segment_writes_filesystem(verb_segment),
            "expected pre-spawn classifier to flag {verb_segment:?} as a filesystem write"
        );
    }

    for destructive_segment in [
        "rm .git/config",
        "dd if=/dev/zero of=.git/HEAD",
        "truncate -s 0 .git/index",
    ] {
        assert!(
            shell_segment_writes_filesystem(destructive_segment),
            "expected pre-spawn classifier to flag destructive {destructive_segment:?}"
        );
    }

    for read_only_segment in ["cat .git/config", "ls .git", "grep foo .git/config"] {
        assert!(
            !shell_segment_writes_filesystem(read_only_segment),
            "expected pre-spawn classifier to leave read-only {read_only_segment:?} alone"
        );
    }
}

#[test]
fn shell_safe_metadata_write_verbs_skip_pre_spawn_gate() {
    use crate::shell_parse::{is_destructive_shell_segment, is_safe_metadata_write_segment};

    for safe in [
        "mkdir /tmp/x",
        "mkdir -p /tmp/nested/dir",
        "chmod 600 src/config",
        "chmod -R 700 build",
        "ln -s /tmp/x /tmp/y",
        "mv tmp/a tmp/b",
        "touch src/lib.rs",
    ] {
        assert!(
            is_safe_metadata_write_segment(safe),
            "expected {safe:?} to clear the pre-spawn safe-verb allowlist"
        );
        assert!(
            !is_destructive_shell_segment(safe),
            "expected {safe:?} to skip the destructive pre-spawn gate"
        );
    }

    for danger in [
        "rm src/lib.rs",
        "rm -rf target",
        "dd if=/dev/zero of=src/lib.rs",
        "truncate -s 0 src/lib.rs",
        "chown -R nobody src",
        // Forced overwrite stays gated even though mv is otherwise safe.
        "mv -f tmp/a tmp/b",
        "mv --force tmp/a tmp/b",
    ] {
        assert!(
            !is_safe_metadata_write_segment(danger),
            "expected {danger:?} to NOT be classified as a safe metadata write"
        );
        assert!(
            is_destructive_shell_segment(danger),
            "expected {danger:?} to trip the destructive pre-spawn gate"
        );
    }

    let mkdir = analyze_shell_command("mkdir /tmp/x");
    assert_eq!(mkdir.capability, PermissionCapability::Edit);
    assert_eq!(mkdir.risk, PermissionRisk::Medium);
    assert!(!mkdir.destructive);

    let rm_star = analyze_shell_command("rm *");
    assert_eq!(rm_star.capability, PermissionCapability::Destructive);
    assert_eq!(rm_star.risk, PermissionRisk::Critical);
    assert!(rm_star.destructive);
}

#[tokio::test]
async fn shell_workdir_accepts_configured_extra_root() {
    let root = temp_workspace("shell_extra_workdir");
    let extra = temp_workspace("shell_extra_root");
    let extra = canonicalize_workspace_root(&extra).expect("canonical extra root");
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
    let audit = fs::read_to_string(root.join(".squeezy/audit/shell.jsonl")).expect("audit log");
    // The audit row JSON-encodes paths with whichever separator the host
    // uses (`\\` on Windows), so assert on the unique basename rather than
    // the full path string.
    let extra_basename = extra
        .file_name()
        .expect("extra basename")
        .to_string_lossy()
        .into_owned();
    assert!(
        audit.contains(&extra_basename),
        "audit log must mention configured extra write root basename {extra_basename}: {audit}"
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
    // Windows does not yet wire up ConPTY for the shell tool, so `tty: true`
    // is documented to degrade to pipe-backed stdio on that platform.
    let expected_tty_output = if cfg!(windows) { "pipe" } else { "tty" };
    assert_eq!(tty.content["stdout"], expected_tty_output);

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
    if cfg!(windows) {
        assert!(socket.starts_with(r"\\.\pipe\"), "{socket}");
    } else {
        assert!(socket.ends_with(".sock"), "{socket}");
        assert!(
            !Path::new(socket).exists(),
            "ask socket should be removed after shell completion"
        );
    }

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

#[tokio::test]
async fn shell_truncation_spills_full_output_to_tempfile_and_round_trips_via_read_tool_output() {
    let root = temp_workspace("shell_truncation_tempfile_spill");
    let registry = registry_with_shell_sandbox_off(&root);

    // 200000 bytes of `x\n` capped at 4096 ensures the captured raw
    // stream both fills the entire byte budget and ALSO drops bytes
    // past the cap, i.e. the path that the F01 spillover-to-tempfile
    // finding is asked to preserve.
    let result = registry
        .execute(
            ToolCall {
                call_id: "call_spillover".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "yes x | head -c 200000",
                    "output_byte_cap": 4096,
                    "description": "exercise tempfile spillover"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["truncated"], true);
    let spillover = result
        .content
        .get("spillover")
        .expect("truncated shell result must carry spillover metadata");
    let spill_path = spillover["path"].as_str().expect("spillover.path");
    let spill_bytes = spillover["bytes"]
        .as_u64()
        .expect("spillover.bytes must be numeric");
    assert!(spill_bytes > 0, "spillover must record non-zero bytes");

    let shaped_stdout = result.content["stdout"].as_str().expect("stdout");
    let expected_footer = format!(
        "[truncated; full output: {spill_path} ({spill_bytes} bytes); recover via read_tool_output {{\"path\": \"{spill_path}\"}}]"
    );
    assert!(
        shaped_stdout.contains(&expected_footer),
        "shaped stdout must surface the spillover path footer naming read_tool_output; got: {shaped_stdout:?}",
    );

    // Path must live under $TMPDIR/squeezy-spillover/<session>/.
    let tmp_base = std::env::temp_dir().join("squeezy-spillover");
    let tmp_canon = tmp_base.canonicalize().expect("canonical tempdir base");
    let spill_canon = PathBuf::from(spill_path)
        .canonicalize()
        .expect("canonical spillover path");
    assert!(
        spill_canon.starts_with(&tmp_canon),
        "spillover path {spill_path:?} must live under {tmp_base:?}",
    );

    // Roundtrip the spillover via read_tool_output { path }.
    let fetched = registry
        .execute(
            ToolCall {
                call_id: "call_read_spillover".to_string(),
                name: "read_tool_output".to_string(),
                arguments: json!({
                    "path": spill_path,
                    "offset": 0,
                    "limit": spill_bytes as usize,
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(fetched.status, ToolStatus::Success);
    assert_eq!(fetched.content["bytes_returned"], spill_bytes);
    assert_eq!(fetched.content["total_bytes"], spill_bytes);
    let content = fetched.content["content"].as_str().expect("content");
    let on_disk_bytes = fs::read(spill_path).expect("spillover file readable");
    assert_eq!(
        content.as_bytes(),
        on_disk_bytes.as_slice(),
        "read_tool_output content must match the spillover file byte-for-byte",
    );
    let raw_captured = result.content.get("output_shape").and_then(|shape| {
        shape
            .get("raw_stdout_bytes")
            .and_then(serde_json::Value::as_u64)
    });
    if let Some(raw_bytes) = raw_captured {
        assert_eq!(
            content.len() as u64,
            raw_bytes,
            "spillover size must match the raw captured stdout (stderr is empty for `yes` output)",
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_over_cap_streams_raw_sidecar_recoverable_via_read_tool_output() {
    let root = temp_workspace("shell_raw_sidecar_recovery");
    let registry = registry_with_shell_sandbox_off(&root);

    // 200000 bytes of `x\n` capped at 4096: the live result keeps 4096 bytes,
    // the capped spillover mirrors only those, but the raw sidecar must hold
    // the FULL pre-cap stream — the bytes the hard cap would otherwise lose.
    let result = registry
        .execute(
            ToolCall {
                call_id: "call_rawsidecar".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "yes x | head -c 200000",
                    "output_byte_cap": 4096,
                    "description": "exercise raw sidecar"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["truncated"], true);

    let raw_spill = result
        .content
        .get("raw_spillover")
        .expect("over-cap shell result must carry a raw_spillover pointer");
    let raw_path = raw_spill["path"].as_str().expect("raw_spillover.path");
    let raw_bytes = raw_spill["bytes"].as_u64().expect("raw_spillover.bytes");
    assert!(
        raw_path.ends_with("-raw.txt"),
        "raw sidecar must use the {{call_id}}-raw suffix: {raw_path:?}",
    );

    // The raw sidecar is a strict superset of the capped spillover.
    let capped = result
        .content
        .get("spillover")
        .expect("capped spillover still present");
    let capped_bytes = capped["bytes"].as_u64().expect("spillover.bytes");
    assert!(
        raw_bytes > capped_bytes,
        "raw sidecar ({raw_bytes}) must hold more than the capped spillover ({capped_bytes})",
    );
    // `yes x | head -c 200000` emits exactly 200000 bytes on stdout.
    assert_eq!(
        raw_bytes, 200_000,
        "raw sidecar must hold the full pre-cap output"
    );

    // The footer points the model at the full pre-cap recovery path.
    let shaped_stdout = result.content["stdout"].as_str().expect("stdout");
    assert!(
        shaped_stdout.contains(&format!("full pre-cap output: {raw_path}"))
            && shaped_stdout.contains("read_tool_output"),
        "footer must name the raw sidecar recovery path: {shaped_stdout:?}",
    );

    // The path lives under the spillover session dir.
    let tmp_base = std::env::temp_dir().join("squeezy-spillover");
    let tmp_canon = tmp_base.canonicalize().expect("canonical tempdir base");
    let raw_canon = PathBuf::from(raw_path)
        .canonicalize()
        .expect("canonical raw sidecar path");
    assert!(
        raw_canon.starts_with(&tmp_canon),
        "raw sidecar {raw_path:?} must live under {tmp_base:?}",
    );

    // Model-initiated recovery: read_tool_output { path } can reach the bytes
    // the hard cap dropped. Read a window past the live cap (offset 50000,
    // well beyond the 4096-byte cap) and under the 25KB tool-spill threshold
    // so the bytes return inline; `total_bytes` proves the full output is
    // reachable.
    const WINDOW: usize = 8_000;
    let offset = 50_000usize;
    let fetched = registry
        .execute(
            ToolCall {
                call_id: "call_read_rawsidecar".to_string(),
                name: "read_tool_output".to_string(),
                arguments: json!({
                    "path": raw_path,
                    "offset": offset,
                    "limit": WINDOW,
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(fetched.status, ToolStatus::Success);
    assert_eq!(
        fetched.content["total_bytes"], raw_bytes,
        "recovery must see the full pre-cap byte count",
    );
    let content = fetched.content["content"].as_str().expect("content");
    let on_disk = fs::read(raw_path).expect("raw sidecar readable");
    assert_eq!(
        content.as_bytes(),
        &on_disk[offset..offset + WINDOW],
        "read_tool_output must return the dropped-by-cap window byte-for-byte",
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_under_cap_writes_no_raw_sidecar() {
    let root = temp_workspace("shell_no_raw_sidecar_under_cap");
    let registry = registry_with_shell_sandbox_off(&root);

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_under_cap".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf 'small output that fits\\n'",
                    "description": "under-cap output writes no raw sidecar"
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["truncated"], false);
    assert!(
        result.content.get("raw_spillover").is_none(),
        "under-cap output must not produce a raw sidecar: {:?}",
        result.content,
    );

    // No `-raw.txt` file may exist anywhere under the spillover tree.
    let spill_base = std::env::temp_dir().join("squeezy-spillover");
    if spill_base.exists() {
        let mut stack = vec![spill_base];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    assert!(
                        !name.ends_with("-raw.txt") || !name.starts_with("call_under_cap"),
                        "under-cap run must leave no raw sidecar: {path:?}",
                    );
                }
            }
        }
    }

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_truncation_records_spillover_path_under_session_dir_for_raw_output_mode() {
    let root = temp_workspace("shell_truncation_raw_mode_spill");
    let registry = registry_with_shell_sandbox_off(&root);

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_raw_spill".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf 'A%.0s' $(seq 1 4096)",
                    "output_byte_cap": 256,
                    "output_mode": "raw",
                    "description": "raw spillover sanity"
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["truncated"], true);
    let spillover = result
        .content
        .get("spillover")
        .expect("raw-mode truncation must also surface a spillover path");
    let spill_path = spillover["path"].as_str().expect("spillover.path");
    assert!(
        PathBuf::from(spill_path).is_file(),
        "spillover file must exist on disk: {spill_path:?}",
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_non_truncated_runs_do_not_spill_to_tempfile() {
    let root = temp_workspace("shell_no_spill_on_clean_run");
    let registry = registry_with_shell_sandbox_off(&root);

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_clean".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf 'hello\\n'",
                    "description": "tiny output that fits"
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["truncated"], false);
    assert!(
        result.content.get("spillover").is_none(),
        "spillover field must be absent when truncation did not fire: {:?}",
        result.content,
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_tool_output_rejects_spillover_paths_outside_the_session_dir() {
    let root = temp_workspace("read_tool_output_path_safety");
    let registry = registry_with_shell_sandbox_off(&root);

    let outside = root.join("not-a-spillover.txt");
    fs::write(&outside, b"forbidden bytes").expect("write outside file");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_escape".to_string(),
                name: "read_tool_output".to_string(),
                arguments: json!({
                    "path": outside.to_string_lossy(),
                    "offset": 0,
                    "limit": 16,
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Error);
    let err = result.content["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("outside the session directory") || err.contains("not found"),
        "expected path-safety rejection, got: {err}",
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_tool_output_rejects_calls_with_both_handle_and_path() {
    let root = temp_workspace("read_tool_output_arg_validation");
    let registry = registry_with_shell_sandbox_off(&root);

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_both".to_string(),
                name: "read_tool_output".to_string(),
                arguments: json!({
                    "handle": "a".repeat(64),
                    "path": "/tmp/somewhere",
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Error);
    let err = result.content["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("exactly one of `handle` or `path`"),
        "expected mutual-exclusion error, got: {err}",
    );

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_neither".to_string(),
                name: "read_tool_output".to_string(),
                arguments: json!({"offset": 0, "limit": 16}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Error);
    let err = result.content["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("requires either `handle` or `path`"),
        "expected missing-arg error, got: {err}",
    );

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
fn test_report_json_scans_past_plain_text_header() {
    let output =
        "Running tests...\n{\"failed\":1,\"passed\":0,\"total\":1,\"message\":\"error: bad\"}\n";

    let shaped = shape_shell_output("pytest --json-report", output, "", false, Some(1));

    assert_eq!(shaped.family, "pytest");
    assert_eq!(shaped.kind, "structured");
    assert!(shaped.stdout.contains("failed=1"));
    assert!(shaped.stdout.contains("error: bad"));
}

#[test]
fn structured_family_plain_text_falls_back_to_line_shaping() {
    let shaped = shape_shell_output(
        "jest --json",
        "Running tests...\nFAIL src/example.test.ts\n",
        "",
        false,
        Some(1),
    );

    assert_eq!(shaped.family, "jest");
    assert_eq!(shaped.kind, "raw_passthrough_shaped");
    assert!(shaped.fallback_reason.is_some());
    assert!(shaped.stdout.contains("FAIL src/example.test.ts"));
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
fn unstructured_shape_drops_leading_whitespace_cargo_progress() {
    // Real cargo output right-pads progress prefixes with leading
    // whitespace; the noise filter has to ignore that whitespace so the
    // lines actually get dropped.
    let output = "   Compiling fnv v1.0.7\n   Downloading crates ...\n     Running tests/foo (target/debug/deps/foo-abc)\n     Finished `test` profile [unoptimized + debuginfo]\n";

    let shaped = shape_shell_output("cargo test", output, "", false, Some(0));

    assert!(
        !shaped.stdout.contains("Compiling fnv"),
        "expected leading-whitespace 'Compiling' line dropped, got: {}",
        shaped.stdout
    );
    assert!(
        !shaped.stdout.contains("Downloading crates"),
        "expected 'Downloading crates' line dropped: {}",
        shaped.stdout
    );
    assert!(
        !shaped.stdout.contains("Running tests/foo"),
        "expected 'Running tests/...' line dropped: {}",
        shaped.stdout
    );
    // "Finished" is signal (it contains a real status) and should survive.
    assert!(
        shaped.stdout.contains("Finished"),
        "expected 'Finished' line preserved: {}",
        shaped.stdout
    );
}

#[test]
fn unstructured_shape_drops_empty_test_result_summaries() {
    // libtest prints "test result: ok. 0 passed; 0 failed; ...; N filtered
    // out" for every cargo-test binary that didn't match the filter. Each
    // empty row is pure noise; only rows with real passes or failures
    // should survive.
    let output = "test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 100 filtered out; finished in 0.00s\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 72 filtered out; finished in 0.00s\ntest result: FAILED. 0 passed; 3 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.01s\n";

    let shaped = shape_shell_output("cargo test entitlement_cache", output, "", false, Some(0));

    assert!(
        shaped.stdout.contains("12 passed"),
        "expected non-empty pass summary preserved: {}",
        shaped.stdout
    );
    assert!(
        shaped.stdout.contains("3 failed"),
        "expected non-empty failure summary preserved: {}",
        shaped.stdout
    );
    assert!(
        !shaped.stdout.contains("0 passed; 0 failed"),
        "expected empty (0 passed, 0 failed) summary dropped: {}",
        shaped.stdout
    );
}

#[test]
fn unstructured_shape_dedupes_identical_signal_lines() {
    // Cargo prints each compiler warning twice (lib target + lib test
    // target). The shaper should fold byte-identical signal lines into
    // one.
    let output = "warning: associated function `is_fresh` is never used\nsome quiet line\nwarning: associated function `is_fresh` is never used\nfinal note\n";

    let shaped = shape_shell_output("cargo test", output, "", false, Some(0));

    let occurrences = shaped.stdout.matches("`is_fresh` is never used").count();
    assert_eq!(
        occurrences, 1,
        "expected duplicate warning collapsed to one occurrence, got {} in: {}",
        occurrences, shaped.stdout
    );
}

#[test]
fn web_tool_config_normalizes_blank_values() {
    let config = WebToolConfig {
        provider: WebSearchProvider::Exa,
        exa_mcp_url: "  ".to_string(),
        exa_api_key: Some("  secret-token  ".to_string()),
        parallel_mcp_url: "  ".to_string(),
        parallel_api_key: Some("  bearer  ".to_string()),
    }
    .normalized();

    assert_eq!(config.exa_mcp_url, DEFAULT_EXA_MCP_URL);
    assert_eq!(config.exa_api_key.as_deref(), Some("secret-token"));
    assert_eq!(config.parallel_mcp_url, DEFAULT_PARALLEL_MCP_URL);
    assert_eq!(config.parallel_api_key.as_deref(), Some("bearer"));
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
fn decode_body_honors_declared_charset() {
    // ISO-8859-1: 0xE9 is `é`, which is not valid standalone UTF-8.
    assert_eq!(decode_body(&[0xE9], "text/html; charset=ISO-8859-1"), "é");
    // windows-1252: 0x92 is a curly apostrophe in the C1 range.
    assert_eq!(
        decode_body(&[0x92], "text/html; charset=windows-1252"),
        "\u{2019}"
    );
    // No charset declared: high bytes decode lossily as before.
    assert_eq!(decode_body(&[0xE9], "text/html"), "\u{FFFD}");
    // Declared UTF-8 round-trips multibyte sequences.
    assert_eq!(
        decode_body("café".as_bytes(), "text/plain; charset=utf-8"),
        "café"
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
            ..WebToolConfig::default()
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
async fn websearch_parallel_provider_dispatches_parallel_mcp_request() {
    let root = temp_workspace("websearch_parallel");
    let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"parallel results"}]}}"#;
    let http = Arc::new(MockWebHttpClient::default());
    http.push_post_response(ok_response("application/json", body.as_bytes()));
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig {
            provider: WebSearchProvider::Parallel,
            parallel_mcp_url: "https://search.parallel.example/mcp".to_string(),
            parallel_api_key: Some("parallel-token".to_string()),
            ..WebToolConfig::default()
        },
        http.clone(),
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_parallel".to_string(),
                name: "websearch".to_string(),
                arguments: json!({"query": "rust async"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["provider"], "parallel");
    assert_eq!(result.content["result"], "parallel results");
    let requests = http.post_requests.lock().expect("post requests");
    assert_eq!(requests[0].url, "https://search.parallel.example/mcp");
    assert!(requests[0].headers.contains(&(
        "authorization".to_string(),
        "Bearer parallel-token".to_string()
    )));
    assert_eq!(requests[0].body["params"]["name"], "web_search");
    assert_eq!(
        requests[0].body["params"]["arguments"]["objective"],
        "rust async"
    );
    assert_eq!(
        requests[0].body["params"]["arguments"]["search_queries"][0],
        "rust async"
    );

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
async fn websearch_surfaces_jsonrpc_error_message() {
    let root = temp_workspace("websearch_jsonrpc_error");
    let http = Arc::new(MockWebHttpClient::default());
    http.push_post_response(ok_response(
        "application/json",
        br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"monthly quota exceeded"}}"#,
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
    let error = result.content["error"].as_str().expect("error");
    assert!(
        error.contains("monthly quota exceeded"),
        "expected provider error message, got: {error}"
    );
    assert!(
        !error.contains("no text content"),
        "JSON-RPC error must not be reported as empty content: {error}"
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
async fn webfetch_refuses_private_ip_target() {
    let root = temp_workspace("webfetch_ssrf");
    let http = Arc::new(MockWebHttpClient::default());
    http.push_get_response(ok_response("text/plain", b"secrets"));
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
                arguments: json!({"url": "http://169.254.169.254/latest/meta-data/"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("internal address")
    );
    // The SSRF guard must short-circuit before any HTTP request is issued.
    assert!(http.get_requests.lock().expect("get requests").is_empty());
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
async fn webfetch_reports_scheme_downgrade_redirect_without_following() {
    let root = temp_workspace("webfetch_redirect_downgrade");
    let http = Arc::new(MockWebHttpClient::default());
    http.push_get_response(redirect_response("http://example.com/next"));
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
    assert_eq!(result.content["redirect_url"], "http://example.com/next");
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
            "notebook_edit",
            "notes_recall",
            "notes_remember",
            "observations",
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
            "checkpoint_check",
            "checkpoint_list",
            "checkpoint_restore_file",
            "checkpoint_revert",
            "checkpoint_show",
            "checkpoint_undo"
        ]
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn tool_specs_avoid_provider_rejected_top_level_schema_keywords() {
    let root = temp_workspace("tool_specs_provider_schema");
    let registry = ToolRegistry::new(&root).expect("registry");

    for spec in registry.specs().iter() {
        let schema = serde_json::to_value(&spec.parameters).expect("schema serializes");
        for keyword in ["oneOf", "anyOf", "allOf", "enum", "not"] {
            assert!(
                schema.get(keyword).is_none(),
                "{} must not put {keyword} at the top level of its provider-facing schema: {schema}",
                spec.name
            );
        }
    }

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
    // Slice-mode reads do not emit a `packets` array; the resolved window
    // is already on the top-level fields, so the content body is the only
    // signal worth asserting on here.
    assert!(read.content["packets"].as_array().is_none());

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
    // The per-packet `claim` string was trimmed from the wire payload; the
    // structured `caller` + `callee` summaries carry the same information.
    assert!(
        upstream.content["packets"]
            .as_array()
            .expect("upstream packets")
            .iter()
            .any(|packet| packet["caller"]["name"].as_str() == Some("run")
                || packet["callee"]["name"].as_str() == Some("run"))
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
async fn read_slice_path_only_succeeds_when_graph_unavailable() {
    // Bug #1: a path-only `read_slice` reads bytes straight off disk and never
    // needs the graph. It must still succeed when the semantic graph is
    // structurally unavailable (slot `None`) or still indexing, instead of
    // returning a `graph_unavailable` result that strands the model.
    let root = temp_workspace("read_slice_path_only_no_graph");
    write_rust_crate(
        &root,
        "pub fn alpha() -> usize { 1 }\npub fn beta() -> usize { 2 }\n",
    );
    let registry = ToolRegistry::new(&root).expect("registry");

    // The registry may open the graph on a background blocking task (a runtime
    // is present under `#[tokio::test]`). Wait for that open to settle before
    // we null the slot, otherwise it could race and repopulate `graph`.
    registry.wait_for_graph_ready(std::time::Duration::from_secs(5));
    // Simulate an unavailable graph: drop the manager and mark the slot ready
    // so dispatch sees a *failed/absent* graph rather than one still indexing.
    *registry.graph.lock().unwrap() = None;
    {
        let (lock, _cv) = &*registry.graph_ready;
        *lock.lock().unwrap() = true;
    }

    let read = registry
        .execute(
            ToolCall {
                call_id: "slice_path".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({"path": "src/lib.rs", "start_line": 1, "end_line": 1}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(read.status, ToolStatus::Success, "{:?}", read.content);
    // A real slice came back (not a graph_unavailable stub).
    assert_eq!(read.content["tool"], json!("read_slice"));
    assert!(read.content.get("graph_available").is_none());
    assert!(
        read.content["content"]
            .as_str()
            .expect("slice content")
            .contains("alpha"),
        "expected first line of source, got {:?}",
        read.content["content"]
    );

    // It must also work while the graph is still *indexing* (slot None, not
    // yet ready) — the path read should not block on or wait for indexing.
    {
        let (lock, _cv) = &*registry.graph_ready;
        *lock.lock().unwrap() = false;
    }
    let read_indexing = registry
        .execute(
            ToolCall {
                call_id: "slice_path_indexing".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({"path": "src/lib.rs", "start_line": 2, "end_line": 2}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        read_indexing.status,
        ToolStatus::Success,
        "{:?}",
        read_indexing.content
    );
    assert_eq!(read_indexing.content["tool"], json!("read_slice"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_slice_symbol_id_still_requires_graph() {
    // Bug #1 guard: the path-only carve-out must NOT leak to `symbol_id`-based
    // read_slice, which genuinely needs the graph. With the graph unavailable,
    // a `symbol_id` read must still report the graph as unavailable.
    let root = temp_workspace("read_slice_symbol_requires_graph");
    write_rust_crate(&root, "pub fn alpha() -> usize { 1 }\n");
    let registry = ToolRegistry::new(&root).expect("registry");

    // Let the background graph open settle before nulling the slot.
    registry.wait_for_graph_ready(std::time::Duration::from_secs(5));
    *registry.graph.lock().unwrap() = None;
    {
        let (lock, _cv) = &*registry.graph_ready;
        *lock.lock().unwrap() = true;
    }

    let read = registry
        .execute(
            ToolCall {
                call_id: "slice_symbol".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({"symbol_id": "does::not::matter"}),
            },
            CancellationToken::new(),
        )
        .await;
    // The graph-gated path is taken: a `graph_unavailable` result, not a slice.
    assert_eq!(read.content["graph_available"], json!(false));
    assert!(read.content.get("content").is_none());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn record_graph_open_preserves_error_instead_of_swallowing() {
    // Bug #7: `record_graph_open` is the folding step the two graph-open
    // construction sites use. On `Err` it must leave the slot `None` AND record
    // the reason — the previous `.ok()` discarded the error, collapsing a
    // *failed* open into the same indistinguishable `None` as an absent graph.
    let error_slot = StdMutex::new(None);
    let failed: squeezy_core::Result<GraphManager> = Err(squeezy_core::SqueezyError::Graph(
        "simulated parser/crawl failure".to_string(),
    ));
    let opened = record_graph_open(failed, &error_slot);
    assert!(opened.is_none(), "a failed open must not yield a manager");
    let recorded = error_slot
        .lock()
        .unwrap()
        .clone()
        .expect("the open error must be recorded, not silently None-ified");
    assert!(
        recorded.contains("simulated parser/crawl failure"),
        "recorded reason should carry the underlying error, got {recorded:?}"
    );
}

#[tokio::test]
async fn graph_open_error_accessor_distinguishes_errored_from_absent() {
    // Bug #7 end-to-end: a healthy workspace records no error, so a non-`None`
    // `graph_open_error()` is unambiguous evidence the open *failed* — even
    // though both an errored and an absent graph leave the slot `None`.
    let root = temp_workspace("graph_open_error_recorded");
    write_rust_crate(&root, "pub fn alpha() -> usize { 1 }\n");
    let registry = ToolRegistry::new(&root).expect("registry");

    // Wait for the background open to complete; a clean open records no error.
    registry.wait_for_graph_ready(std::time::Duration::from_secs(5));
    assert!(
        registry.graph_open_error().is_none(),
        "successful open must not record an error"
    );

    // An errored open (slot `None` + recorded reason) is distinguishable from
    // a legitimately-absent graph (slot `None`, no reason) via the accessor.
    *registry.graph.lock().unwrap() = None;
    *registry.graph_open_error.lock().unwrap() = Some("simulated parser failure".to_string());
    assert_eq!(
        registry.graph_open_error().as_deref(),
        Some("simulated parser failure"),
        "open error must be preserved and surfaced by the accessor"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn repo_map_truncates_when_single_root_exceeds_cap() {
    // Bug (medium): the cap counted root nodes, not total serialized children.
    // One wide root (a file with many top-level symbols) can blow past
    // `max_files` in the serialized output while `nodes.len() == 1`, which used
    // to report `truncated=false`.
    let root = temp_workspace("repo_map_wide_root_truncates");
    let mut source = String::new();
    for i in 0..12 {
        source.push_str(&format!("pub fn func_{i}() -> usize {{ {i} }}\n"));
    }
    write_rust_crate(&root, &source);
    let registry = ToolRegistry::new(&root).expect("registry");

    let repo_map = registry
        .execute(
            ToolCall {
                call_id: "repo_map_cap".to_string(),
                name: "repo_map".to_string(),
                // One file root with 12 functions; cap at 3 serialized nodes.
                arguments: json!({"max_depth": 2, "max_files": 3}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        repo_map.status,
        ToolStatus::Success,
        "{:?}",
        repo_map.content
    );
    if repo_map.content["graph_available"].as_bool() != Some(true) {
        // No graph available in this environment — the truncation path is a
        // no-op; nothing to assert.
        let _ = fs::remove_dir_all(root);
        return;
    }
    assert_eq!(
        repo_map.content["truncated"],
        json!(true),
        "a single root with more serialized children than the cap must truncate: {:?}",
        repo_map.content
    );

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

/// Collect the declaration names from a `decl_search` result's packets.
fn decl_search_packet_names(content: &Value) -> Vec<String> {
    content["packets"]
        .as_array()
        .map(|packets| {
            packets
                .iter()
                .filter_map(|p| p["symbol"]["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn decl_search_transitive_returns_full_subtype_closure() {
    // A 3-level C# hierarchy: A <- B <- C. The graph only records each class's
    // DIRECT base (`B` carries `base:A`, `C` carries `base:B`, not `base:A`),
    // so a one-shot `attribute="base:A"` query surfaces only B. With
    // `transitive=true` the closure must walk B -> C and return BOTH.
    let root = temp_workspace("decl_search_transitive_closure");
    fs::write(
        root.join("Hierarchy.cs"),
        r#"
namespace App;

public class A { }
public class B : A { }
public class C : B { }
"#,
    )
    .expect("write csharp source");
    let registry = ToolRegistry::new(&root).expect("registry");

    // Sanity: the non-transitive search returns ONLY the direct subtype B.
    let direct = registry
        .execute(
            ToolCall {
                call_id: "decl_direct".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({ "attribute": "base:A" }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(direct.status, ToolStatus::Success, "{:?}", direct.content);
    let direct_names = decl_search_packet_names(&direct.content);
    assert_eq!(
        direct_names,
        vec!["B".to_string()],
        "direct (transitive absent) decl_search must return only the immediate subtype: {:?}",
        direct.content
    );

    // transitive=false behaves identically to omitting it.
    let explicit_false = registry
        .execute(
            ToolCall {
                call_id: "decl_false".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({ "attribute": "base:A", "transitive": false }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        explicit_false.status,
        ToolStatus::Success,
        "{:?}",
        explicit_false.content
    );
    assert_eq!(
        decl_search_packet_names(&explicit_false.content),
        vec!["B".to_string()],
        "transitive=false must match the omitted-flag behaviour: {:?}",
        explicit_false.content
    );

    // transitive=true walks the whole subtype tree: BOTH B and C.
    let transitive = registry
        .execute(
            ToolCall {
                call_id: "decl_transitive".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({ "attribute": "base:A", "transitive": true }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        transitive.status,
        ToolStatus::Success,
        "{:?}",
        transitive.content
    );
    let transitive_names = decl_search_packet_names(&transitive.content);
    assert!(
        transitive_names.contains(&"B".to_string()) && transitive_names.contains(&"C".to_string()),
        "transitive=true must return the full subtype closure (B and C), got {transitive_names:?}: {:?}",
        transitive.content
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn decl_search_transitive_walks_mixed_kind_chain() {
    // Mixed-kind chain: `class C` reaches base interface `I` only THROUGH an
    // intermediate interface `J` (`I` <- iface `J` <- class `C`). A
    // `kind=class, attribute=base:I, transitive=true` query must still return
    // `C`: the closure has to walk the interface intermediate (a *different*
    // kind) and apply `kind=class` to the EMITTED results, not to the walk.
    // Before the fix the walk threaded `kind=class` into every expansion, so
    // the interface `J` was never enqueued and `C`'s subtree was dropped.
    let root = temp_workspace("decl_search_transitive_mixed_kind");
    fs::write(
        root.join("Mixed.cs"),
        r#"
namespace App;

public interface I { }
public interface J : I { }
public class C : J { }
"#,
    )
    .expect("write csharp source");
    let registry = ToolRegistry::new(&root).expect("registry");

    let transitive = registry
        .execute(
            ToolCall {
                call_id: "decl_mixed".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({ "attribute": "base:I", "kind": "class", "transitive": true }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        transitive.status,
        ToolStatus::Success,
        "{:?}",
        transitive.content
    );
    let names = decl_search_packet_names(&transitive.content);
    assert!(
        names.contains(&"C".to_string()),
        "transitive closure must reach class C THROUGH interface J (kind applied \
         to emitted results, not the walk), got {names:?}: {:?}",
        transitive.content
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn decl_search_transitive_honors_query_filter() {
    // `query` must narrow a transitive closure exactly as it narrows a one-shot
    // search. base:A's closure is {B, Admin, SuperAdmin}; adding query="Admin"
    // must drop the unrelated sibling B and keep the Admin branch. Before the
    // fix the transitive branch ignored `query` entirely and returned the whole
    // closure.
    let root = temp_workspace("decl_search_transitive_query");
    fs::write(
        root.join("Roles.cs"),
        r#"
namespace App;

public class A { }
public class B : A { }
public class Admin : A { }
public class SuperAdmin : Admin { }
"#,
    )
    .expect("write csharp source");
    let registry = ToolRegistry::new(&root).expect("registry");

    // Without query: the whole closure includes the unrelated sibling B.
    let all = registry
        .execute(
            ToolCall {
                call_id: "decl_all".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({ "attribute": "base:A", "transitive": true }),
            },
            CancellationToken::new(),
        )
        .await;
    let all_names = decl_search_packet_names(&all.content);
    assert!(
        all_names.contains(&"B".to_string()),
        "unfiltered transitive closure should include B: {all_names:?}"
    );

    // With query="Admin": B is dropped, the Admin branch kept.
    let narrowed = registry
        .execute(
            ToolCall {
                call_id: "decl_query".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({ "attribute": "base:A", "query": "Admin", "transitive": true }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        narrowed.status,
        ToolStatus::Success,
        "{:?}",
        narrowed.content
    );
    let narrowed_names = decl_search_packet_names(&narrowed.content);
    assert!(
        narrowed_names.contains(&"Admin".to_string()),
        "query=Admin must keep the Admin branch: {narrowed_names:?}"
    );
    assert!(
        !narrowed_names.contains(&"B".to_string()),
        "query=Admin must drop the unrelated sibling B (returned without query): {narrowed_names:?}"
    );

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
    assert!(
        result.content["fallback"].get("suggested_tools").is_none(),
        "fallback.suggested_tools must be trimmed from the wire payload"
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
    // The per-packet `next_action` recommendation was trimmed from the wire
    // payload; the model can derive a `read_slice` call from the embedded
    // symbol id + span without an explicit recommendation.

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
    // The trim drops the per-packet `claim` field; the call-chain packet is
    // still uniquely identifiable by its `chain` array — call_edge / edge
    // packets do not carry that shape.
    assert!(
        chain_packets
            .iter()
            .any(|packet| packet.get("chain").and_then(|v| v.as_array()).is_some()),
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
async fn reference_search_emits_confidence_distribution() {
    let root = temp_workspace("graph_reference_confidence_distribution");
    fs::create_dir_all(root.join("services")).expect("create services dir");
    fs::write(
        root.join("services").join("greeter.py"),
        r#"
class Greeter:
    def hello(self):
        return "hi"

# Same-file reference to Greeter — binds via the loose-reference path and
# therefore lands in the `Heuristic` bucket.
def make_local():
    return Greeter()
"#,
    )
    .expect("write greeter");
    // Cross-file alias-import reference: `Greeter` is brought in via `as GA`,
    // calls through that alias bind via `reference_alias_matches_symbol` and
    // therefore land in the `ImportResolved` bucket.
    fs::write(
        root.join("aliased.py"),
        r#"
from services.greeter import Greeter as GA

def make_aliased():
    return GA()
"#,
    )
    .expect("write aliased");

    let registry = ToolRegistry::new(&root).expect("registry");

    let definition = registry
        .execute(
            ToolCall {
                call_id: "ref_distribution_def".to_string(),
                name: "definition_search".to_string(),
                arguments: json!({"query": "Greeter", "path": "services/greeter.py"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(definition.status, ToolStatus::Success);
    let greeter_id = definition.content["packets"][0]["symbol"]["id"]
        .as_str()
        .expect("definition packet carries symbol id")
        .to_string();

    let result = registry
        .execute(
            ToolCall {
                call_id: "ref_distribution".to_string(),
                name: "reference_search".to_string(),
                arguments: json!({"symbol_id": greeter_id}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);

    let distribution = &result.cost_hint.confidence_distribution;
    assert!(
        !distribution.is_empty(),
        "reference_search must populate confidence_distribution, got {result:?}"
    );

    let valid_ids: std::collections::HashSet<&str> = squeezy_core::Confidence::ALL
        .iter()
        .map(|c| c.id())
        .collect();
    for key in distribution.keys() {
        assert!(
            valid_ids.contains(key.as_str()),
            "distribution key `{key}` is not a Confidence::id() value"
        );
    }

    let packets = result.content["packets"]
        .as_array()
        .expect("reference_search packets array");
    let total: u32 = distribution.values().copied().sum();
    assert_eq!(
        total as usize,
        packets.len(),
        "distribution counts must sum to the number of returned packets"
    );

    let mut expected: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for packet in packets {
        // Confidence now lives in the `reference` body (the top-level mirror was
        // dropped to cut duplicate tokens), so read it from there.
        let label = packet["reference"]["confidence"]
            .as_str()
            .expect("reference packet must carry a confidence label")
            .to_string();
        *expected.entry(label).or_insert(0) += 1;
    }
    assert_eq!(
        distribution, &expected,
        "cost_hint distribution must match per-packet confidence counts"
    );

    assert!(
        distribution.len() >= 2,
        "fixture must produce a mix of confidence buckets, got {distribution:?}"
    );
    assert!(
        distribution.contains_key("import_resolved"),
        "fixture must surface at least one import_resolved reference, got {distribution:?}"
    );
    assert!(
        distribution.contains_key("heuristic"),
        "fixture must surface at least one heuristic reference, got {distribution:?}"
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
    // `suggested_tools` is intentionally dropped from the wire payload — the
    // reason code carries the load-bearing signal and the model can choose a
    // retry tool without a verbose recommendation list.
    assert!(
        fallback.get("suggested_tools").is_none(),
        "fallback.suggested_tools must be trimmed from the wire payload, got {fallback}"
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
    assert!(
        fallback.get("suggested_tools").is_none(),
        "fallback.suggested_tools must be trimmed from the wire payload, got {fallback}"
    );

    let _ = fs::remove_dir_all(root);
}

// `decl_search_zero_hit_regex_escapes_query` was removed when
// `fallback.suggested_tools` was trimmed from the wire payload. The dropped
// field was the only consumer of the regex-escape path; the reason code stays
// and the model picks its own retry tool.

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
        .find(|packet| packet["edge"]["confidence"].as_str() == Some("candidate_set"))
        .unwrap_or_else(|| panic!("expected at least one candidate_set packet: {packets:?}"));
    let candidates = candidate_packet["candidates"]
        .as_array()
        .unwrap_or_else(|| {
            panic!("CandidateSet packet must include candidates: {candidate_packet}")
        });
    assert_eq!(candidates.len(), 2);
    for entry in candidates {
        assert_eq!(entry["name"].as_str(), Some("do_thing"));
    }
    // The per-packet `next_action.fanout` recommendation was trimmed from the
    // wire payload. The model still gets every candidate's symbol id + span
    // through the `candidates` array above; the read_slice retry shape was
    // duplicated decoration.
    assert!(
        candidate_packet.get("next_action").is_none(),
        "candidate-set packet must not carry trimmed next_action: {candidate_packet}"
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

fn sample_notebook_bytes() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "cells": [
            {
                "cell_type": "code",
                "id": "alpha",
                "execution_count": 7,
                "metadata": {},
                "outputs": [{"output_type": "stream", "text": "stale\n"}],
                "source": ["print('old')\n"]
            },
            {
                "cell_type": "markdown",
                "id": "beta",
                "metadata": {},
                "source": ["# heading\n"]
            }
        ],
        "metadata": {},
        "nbformat": 4,
        "nbformat_minor": 5
    }))
    .expect("notebook bytes")
}
#[tokio::test]
async fn notebook_edit_resets_execution_count_for_code_cell() {
    let root = temp_workspace("notebook_edit_code");
    let bytes = sample_notebook_bytes();
    fs::write(root.join("nb.ipynb"), &bytes).expect("write notebook");
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "nb_replace".to_string(),
                name: "notebook_edit".to_string(),
                arguments: json!({
                    "path": "nb.ipynb",
                    "cell_id": "alpha",
                    "new_source": "print('new')\n",
                    "expected_sha256": sha256_hex(&bytes),
                }),
            },
            CancellationToken::new(),
            "turn-nb".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let on_disk = fs::read(root.join("nb.ipynb")).expect("re-read");
    let parsed: Value = serde_json::from_slice(&on_disk).expect("valid JSON");
    let alpha = &parsed["cells"][0];
    assert_eq!(alpha["execution_count"], Value::Null);
    assert_eq!(alpha["outputs"].as_array().expect("outputs").len(), 0);
    assert_eq!(alpha["source"][0], "print('new')\n");
    assert_eq!(result.content["edit"]["mode"], "replace");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn notebook_edit_replaces_cell_by_id_preserves_outputs_array_for_markdown() {
    let root = temp_workspace("notebook_edit_md");
    let bytes = sample_notebook_bytes();
    fs::write(root.join("nb.ipynb"), &bytes).expect("write notebook");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "nb_md".to_string(),
                name: "notebook_edit".to_string(),
                arguments: json!({
                    "path": "nb.ipynb",
                    "cell_id": "beta",
                    "new_source": "# new heading\n",
                    "expected_sha256": sha256_hex(&bytes),
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let on_disk = fs::read(root.join("nb.ipynb")).expect("re-read");
    let parsed: Value = serde_json::from_slice(&on_disk).expect("valid JSON");
    let beta = &parsed["cells"][1];
    // Markdown cells should NOT acquire execution_count/outputs fields.
    assert!(beta.get("execution_count").is_none());
    assert!(beta.get("outputs").is_none());
    assert_eq!(beta["source"][0], "# new heading\n");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn notebook_edit_inserts_new_cell_at_position() {
    let root = temp_workspace("notebook_edit_insert");
    let bytes = sample_notebook_bytes();
    fs::write(root.join("nb.ipynb"), &bytes).expect("write notebook");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "nb_ins".to_string(),
                name: "notebook_edit".to_string(),
                arguments: json!({
                    "path": "nb.ipynb",
                    "cell_id": "alpha",
                    "edit_mode": "insert",
                    "cell_type": "code",
                    "new_source": "x = 1\n",
                    "expected_sha256": sha256_hex(&bytes),
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let on_disk = fs::read(root.join("nb.ipynb")).expect("re-read");
    let parsed: Value = serde_json::from_slice(&on_disk).expect("valid JSON");
    let cells = parsed["cells"].as_array().expect("cells");
    assert_eq!(cells.len(), 3);
    assert_eq!(cells[1]["cell_type"], "code");
    assert_eq!(cells[1]["source"][0], "x = 1\n");
    assert_eq!(cells[1]["execution_count"], Value::Null);
    assert!(cells[1]["outputs"].as_array().expect("outputs").is_empty());
    assert_eq!(result.content["edit"]["cell_index"], 1);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn notebook_edit_deletes_cell_and_writes_valid_json() {
    let root = temp_workspace("notebook_edit_delete");
    let bytes = sample_notebook_bytes();
    fs::write(root.join("nb.ipynb"), &bytes).expect("write notebook");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "nb_del".to_string(),
                name: "notebook_edit".to_string(),
                arguments: json!({
                    "path": "nb.ipynb",
                    "cell_id": "alpha",
                    "edit_mode": "delete",
                    "expected_sha256": sha256_hex(&bytes),
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let on_disk = fs::read(root.join("nb.ipynb")).expect("re-read");
    let parsed: Value = serde_json::from_slice(&on_disk).expect("valid JSON");
    let cells = parsed["cells"].as_array().expect("cells");
    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0]["id"], "beta");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_refuses_ipynb_with_redirect_to_notebook_edit() {
    let root = temp_workspace("apply_patch_ipynb");
    let bytes = sample_notebook_bytes();
    fs::write(root.join("nb.ipynb"), &bytes).expect("write notebook");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "patch_nb".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "patches": [{
                        "path": "nb.ipynb",
                        "search": "old",
                        "replace": "new",
                        "expected_sha256": sha256_hex(&bytes),
                    }]
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(result.content["suggested_tool"], "notebook_edit");
    // File untouched.
    assert_eq!(fs::read(root.join("nb.ipynb")).unwrap(), bytes);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn write_file_refuses_ipynb_with_redirect_to_notebook_edit() {
    let root = temp_workspace("write_file_ipynb");
    let bytes = sample_notebook_bytes();
    fs::write(root.join("nb.ipynb"), &bytes).expect("write notebook");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "write_nb".to_string(),
                name: "write_file".to_string(),
                arguments: json!({
                    "path": "nb.ipynb",
                    "content": "{}",
                    "expected_sha256": sha256_hex(&bytes),
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(result.content["suggested_tool"], "notebook_edit");
    assert_eq!(fs::read(root.join("nb.ipynb")).unwrap(), bytes);
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
    // After the packet-slimming trim, the top-level `spans`/`confidence` mirror
    // is dropped for packets that carry a body field (symbol/reference/edge):
    // the body already re-encodes the same span(s) + confidence, so duplicating
    // them at the top level was pure token overhead. Only the two bare-packet
    // shapes (read_slice diff packets and the unresolved hierarchy-node
    // fallback) — which have no body field — still carry a minimal top-level
    // `spans` + `confidence`, because that is the only path+span the model has.
    //
    // Invariant asserted here: a packet either (a) carries a body field
    // (symbol/reference/edge) that bears a `confidence`, with NO top-level
    // `spans`/`confidence`, or (b) is a bare packet that carries top-level
    // `spans` + `confidence` and no body field.
    let body_field = ["symbol", "reference", "edge"]
        .into_iter()
        .find(|key| packet.get(key).is_some());
    if let Some(field) = body_field {
        assert!(
            packet[field].get("confidence").is_some(),
            "body field `{field}` must carry confidence: {packet}"
        );
        for absent in ["spans", "confidence"] {
            assert!(
                packet.get(absent).is_none(),
                "body-bearing evidence packet must not mirror `{absent}` at the top level: {packet}"
            );
        }
    } else {
        for key in ["spans", "confidence"] {
            assert!(
                packet.get(key).is_some(),
                "bare evidence packet missing key {key}: {packet}"
            );
        }
    }
    for trimmed in [
        "claim",
        "freshness",
        "provenance",
        "cost_hint",
        "next_action",
    ] {
        assert!(
            packet.get(trimmed).is_none(),
            "evidence packet must not carry trimmed key {trimmed}: {packet}"
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

    // Redirecting noise to /dev/null is stderr/stdout suppression, not a
    // destructive filesystem write.
    let sonar_stderr_null = analyze_shell_command("sonar context list --json 2>/dev/null");
    assert_eq!(sonar_stderr_null.capability, PermissionCapability::Shell);
    assert!(!sonar_stderr_null.destructive);

    let cargo_stdout_null = analyze_shell_command("cargo test >/dev/null");
    assert_eq!(cargo_stdout_null.capability, PermissionCapability::Compiler);
    assert!(!cargo_stdout_null.destructive);

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

    // Near misses still count as real file writes.
    let dev_null_suffix = analyze_shell_command("echo hi >/dev/null.log");
    assert_eq!(
        dev_null_suffix.capability,
        PermissionCapability::Destructive
    );
    assert!(dev_null_suffix.destructive);
}

#[test]
fn plan_mode_shell_read_only_classifier_blocks_repo_mutators() {
    assert!(plan_mode_shell_command_is_read_only(
        "sonar context guidelines get --languages java 2>/dev/null"
    ));
    assert!(plan_mode_shell_command_is_read_only(
        "find . -name \"*.java\" -not -path \"*/target/*\" | head -60"
    ));
    assert!(plan_mode_shell_command_is_read_only(
        "sonar context navigation search-signatures --pattern \".*\" --fields \"fqn,file_path,start_line\" --limit 20 2>/dev/null | python3 -c \"import sys,json; d=json.load(sys.stdin); [print(x['fqn'],'->',x['file_path']) for x in d.get('results',[])]\" 2>/dev/null || true"
    ));
    assert!(plan_mode_shell_command_is_read_only("cargo fmt --check"));
    assert!(plan_mode_shell_command_is_read_only(
        "cargo test -p squeezy-agent"
    ));
    assert!(plan_mode_shell_command_is_read_only("git status --short"));
    assert!(plan_mode_shell_command_is_read_only("git diff -- crates"));

    assert!(!plan_mode_shell_command_is_read_only("cargo fmt"));
    assert!(!plan_mode_shell_command_is_read_only("cargo clippy --fix"));
    assert!(!plan_mode_shell_command_is_read_only(
        "git diff --output=/private/tmp/sqz-pr364-diff-output-check origin/main...HEAD"
    ));
    assert!(!plan_mode_shell_command_is_read_only(
        "git diff --output diff.patch origin/main...HEAD"
    ));
    assert!(!plan_mode_shell_command_is_read_only("git checkout -b x"));
    assert!(!plan_mode_shell_command_is_read_only("git branch x"));
    assert!(!plan_mode_shell_command_is_read_only(
        "echo $(touch created.txt)"
    ));
    assert!(!plan_mode_shell_command_is_read_only(
        "cat <(touch created.txt)"
    ));
    assert!(!plan_mode_shell_command_is_read_only("make test"));
    assert!(!plan_mode_shell_command_is_read_only("node script.js"));
    assert!(!plan_mode_shell_command_is_read_only(
        "sort -o out.txt input.txt"
    ));
    assert!(!plan_mode_shell_command_is_read_only(
        "uniq input.txt out.txt"
    ));
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
        best_effort_fallback: None,
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
    let shell = ShellProgram::for_command("printf ok");
    assert_eq!(plan.program, shell.program);
    assert_eq!(plan.args, shell.args);
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
fn macos_sandbox_profile_denies_af_unix_when_network_denied() {
    let root = temp_workspace("macos_profile_af_unix_denied");
    let profile = macos_shell_sandbox_profile(&root, &ShellSandboxConfig::default(), false);

    // With network denied and the default empty AF_UNIX allowlist, the
    // profile must not emit any allow-network rule. The default
    // `(deny default)` then keeps AF_UNIX blocked.
    assert!(
        !profile.contains("(allow network"),
        "denied-network profile must not allow AF_UNIX sockets: {profile}"
    );
    assert!(
        !profile.contains("(local unix)"),
        "stale `(local unix)` rule should be removed: {profile}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
#[cfg(target_os = "macos")]
fn macos_sandbox_profile_allows_full_network_when_network_allowed() {
    let root = temp_workspace("macos_profile_network_allowed");
    let profile = macos_shell_sandbox_profile(&root, &ShellSandboxConfig::default(), true);

    // When network is permitted the existing wildcard allow remains so
    // that classified-network commands keep working unchanged.
    assert!(
        profile.contains("(allow network*)"),
        "allow-network profile must keep the wildcard rule: {profile}"
    );

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
        raw_spillover: None,
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
        raw_spillover: None,
    };

    let reason = shell_sandbox_best_effort_fallback_reason(&plan, &run)
        .expect("best effort runtime fallback reason");

    assert!(reason.contains("failed at runtime"), "{reason}");
    assert!(reason.contains("best_effort"), "{reason}");
}

#[test]
fn shell_sandbox_health_counts_fallbacks_and_latches_warning() {
    // F3-4: the tools layer is the source of truth for the
    // `approval.best_effort.fallback{tool=shell}` counter AND the
    // one-shot TUI warning. The counter ticks every time; the latch
    // flips to "not first" after the first call.
    let health = ShellSandboxHealth::default();

    assert_eq!(health.best_effort_fallback_count(), 0);

    let first = health.record_best_effort_fallback();
    assert_eq!(first.fallback_count, 1);
    assert!(
        first.first_in_session,
        "first fallback in a session must surface the warning"
    );

    let second = health.record_best_effort_fallback();
    assert_eq!(second.fallback_count, 2, "counter must keep ticking");
    assert!(
        !second.first_in_session,
        "subsequent fallbacks must NOT re-fire the one-shot warning"
    );

    let third = health.record_best_effort_fallback();
    assert_eq!(third.fallback_count, 3);
    assert!(!third.first_in_session);

    assert_eq!(health.best_effort_fallback_count(), 3);
}

#[test]
fn shell_sandbox_plan_metadata_carries_best_effort_fallback_record() {
    // The fallback record reaches the agent layer via the `sandbox`
    // JSON in `ToolResult.content`; we round-trip it here to lock in
    // the schema the agent reads in `shell_best_effort_fallback_from_result`.
    let health = ShellSandboxHealth::default();
    let record = health.record_best_effort_fallback();

    let config = sandbox_config(
        ShellSandboxMode::BestEffort,
        ShellSandboxNetworkPolicy::DenyByDefault,
    );
    let plan = ShellSandboxPlan::direct_with_fallback_record(
        "printf ok",
        ShellSandboxMode::BestEffort,
        &config,
        Some("backend disabled".to_string()),
        Some(("macos-sandbox-exec", record)),
    );

    let metadata = plan.metadata();
    let fallback = metadata
        .get("best_effort_fallback")
        .expect("best_effort_fallback present in metadata");
    assert_eq!(
        fallback.get("backend").and_then(Value::as_str),
        Some("macos-sandbox-exec")
    );
    assert_eq!(
        fallback.get("fallback_count").and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        fallback.get("first_in_session").and_then(Value::as_bool),
        Some(true)
    );

    // The public agent-facing helper round-trips the same payload off a
    // synthetic ToolResult.
    let result = ToolResult {
        call_id: "call".to_string(),
        tool_name: "shell".to_string(),
        status: ToolStatus::Success,
        content: json!({ "sandbox": metadata }),
        cost_hint: ToolCostHint::default(),
        receipt: ToolReceipt {
            output_sha256: "0".repeat(64),
            content_sha256: None,
        },
        spill_model_output: None,
        web_call_stats: None,
    };
    let parsed = shell_best_effort_fallback_from_result(&result)
        .expect("agent helper extracts the fallback descriptor");
    assert_eq!(parsed.backend, "macos-sandbox-exec");
    assert_eq!(parsed.fallback_count, 1);
    assert!(parsed.first_in_session);
}

#[test]
fn shell_best_effort_fallback_from_result_ignores_non_shell_tools_and_clean_runs() {
    // Defence in depth: the agent layer must only fire its one-shot
    // warning for shell calls, and only when the sandbox actually
    // degraded. Read_file or a clean shell call must return `None`.
    let plain_result = ToolResult {
        call_id: "call".to_string(),
        tool_name: "shell".to_string(),
        status: ToolStatus::Success,
        content: json!({
            "sandbox": {
                "backend": "macos-sandbox-exec",
                "mode": "best_effort",
            }
        }),
        cost_hint: ToolCostHint::default(),
        receipt: ToolReceipt {
            output_sha256: "0".repeat(64),
            content_sha256: None,
        },
        spill_model_output: None,
        web_call_stats: None,
    };
    assert!(shell_best_effort_fallback_from_result(&plain_result).is_none());

    let non_shell = ToolResult {
        call_id: "call".to_string(),
        tool_name: "read_file".to_string(),
        status: ToolStatus::Success,
        content: json!({
            "sandbox": {
                "best_effort_fallback": {
                    "backend": "macos-sandbox-exec",
                    "fallback_count": 1,
                    "first_in_session": true,
                }
            }
        }),
        cost_hint: ToolCostHint::default(),
        receipt: ToolReceipt {
            output_sha256: "0".repeat(64),
            content_sha256: None,
        },
        spill_model_output: None,
        web_call_stats: None,
    };
    assert!(
        shell_best_effort_fallback_from_result(&non_shell).is_none(),
        "non-shell tools must not trip the shell sandbox warning"
    );
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
fn linux_seccomp_plan_does_not_export_ask_socket() {
    // The linux-direct-syscalls seccomp filter denies socket(AF_UNIX, …),
    // so `squeezy ask` could never connect; the socket must not be exported.
    let seccomp_plan = fake_sandbox_plan("linux-direct-syscalls", true);
    assert!(!seccomp_plan.exports_ask_socket());

    // Backends without the AF_UNIX deny still advertise the ask socket.
    for backend in [
        "none",
        "external",
        "macos-sandbox-exec",
        "windows-job-object",
    ] {
        let plan = fake_sandbox_plan(backend, false);
        assert!(
            plan.exports_ask_socket(),
            "backend {backend} should export the ask socket",
        );
    }
}

#[test]
fn grep_spec_promotes_graph_first() {
    let description = grep_spec().description;
    for marker in [
        "decl_search",
        "reference_search",
        "symbol_context",
        "imports and re-exports",
    ] {
        assert!(
            description.contains(marker),
            "grep_spec must mention `{marker}`; got: {description}"
        );
    }
    // Earlier this enumerated every language from `LanguageFamily::all()` in
    // the description. Coverage grew to ~14 mainstream families and the
    // per-prompt token cost outweighed the guidance value, so the prose now
    // says "indexed source files" instead. The golden-file assertion below
    // is the canonical pin.
    let golden =
        include_str!("../tests/artifacts/tool-spec-descriptions/grep_spec_description.txt").trim();
    assert_eq!(description.trim(), golden);
}

#[test]
fn glob_spec_promotes_graph_first() {
    let description = glob_spec().description;
    assert!(description.contains("decl_search"));
    assert!(description.contains("imports and re-exports"));
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

#[tokio::test]
async fn observations_tool_lists_recent_and_searches_existing_store() {
    use squeezy_store::{Observation, ObservationKind};

    let root = temp_workspace("observations_tool");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("open store"));

    let seeded = [
        (
            ObservationKind::Decision,
            "Prefer ripgrep over grep for workspace search.",
            vec!["search", "tooling"],
            "audit",
        ),
        (
            ObservationKind::Convention,
            "Public APIs document units explicitly.",
            vec!["docs"],
            "convention-log",
        ),
        (
            ObservationKind::DeadEnd,
            "Attempted to vendor libgit2; build broke on Windows.",
            vec!["build", "git"],
            "post-mortem",
        ),
    ];
    let mut put_ids = Vec::new();
    for (kind, text, tags, source) in seeded.iter() {
        let mut obs = Observation::new(*kind, *text, *source);
        obs.tags = tags.iter().map(|tag| (*tag).to_string()).collect();
        let stored = store.put_observation(obs).expect("put observation");
        put_ids.push(stored.id);
    }

    let registry = registry_with_state_store(&root, store.clone());

    // Default (no query) returns newest-first, capped by `limit`.
    let listed = registry
        .execute(
            ToolCall {
                call_id: "obs_list".to_string(),
                name: "observations".to_string(),
                arguments: json!({}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(listed.status, ToolStatus::Success);
    let listed_items = listed.content["observations"]
        .as_array()
        .expect("observations array");
    assert_eq!(listed_items.len(), seeded.len());
    let first = &listed_items[0];
    assert_eq!(first["id"], put_ids[2]);
    assert_eq!(first["kind"], "deadend");
    assert_eq!(first["summary"], seeded[2].1);
    assert_eq!(first["tags"], json!(["build", "git"]));
    assert!(first["timestamp"].is_number());
    // Recency ordering: newest seeded first, oldest last.
    let ordered_ids: Vec<&str> = listed_items
        .iter()
        .map(|item| item["id"].as_str().expect("id string"))
        .collect();
    let expected_recent: Vec<&str> = put_ids.iter().rev().map(String::as_str).collect();
    assert_eq!(ordered_ids, expected_recent);

    // Query path delegates to `search_observations` and honours `limit`.
    let searched = registry
        .execute(
            ToolCall {
                call_id: "obs_search".to_string(),
                name: "observations".to_string(),
                arguments: json!({"query": "ripgrep", "limit": 5}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(searched.status, ToolStatus::Success);
    let searched_items = searched.content["observations"]
        .as_array()
        .expect("observations array");
    assert_eq!(searched_items.len(), 1);
    assert_eq!(searched_items[0]["id"], put_ids[0]);
    assert_eq!(searched_items[0]["kind"], "decision");
    assert_eq!(searched_items[0]["summary"], seeded[0].1);
}

#[tokio::test]
async fn observations_tool_fails_without_store_handle() {
    let root = temp_workspace("observations_no_store");
    let registry = registry_with_runtime_config(&root, ToolRuntimeConfig::default());
    let result = registry
        .execute(
            ToolCall {
                call_id: "obs_no_store".to_string(),
                name: "observations".to_string(),
                arguments: json!({}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Error);
}

#[test]
fn human_label_renders_one_phrase_for_known_tools() {
    let cases: &[(&str, Value, &str)] = &[
        (
            "definition_search",
            json!({"kind": "struct", "query": "Error", "path": "src/lib.rs"}),
            "looking up struct `Error` in `src/lib.rs`",
        ),
        (
            "symbol_context",
            json!({"query": "Squeezy::run"}),
            "getting context for `Squeezy::run`",
        ),
        (
            "reference_search",
            json!({"symbol_id": "crate::foo::Bar"}),
            "finding references to `crate::foo::Bar`",
        ),
        (
            "reference_search",
            json!({"query": "Bar"}),
            "finding references to `Bar`",
        ),
        (
            "downstream_flow",
            json!({"query": "from_path", "max_depth": 3}),
            "tracing flow downstream from `from_path`",
        ),
        (
            "upstream_flow",
            json!({"query": "from_path"}),
            "tracing flow upstream from `from_path`",
        ),
        (
            "repo_map",
            json!({"max_depth": 4}),
            "building a repo map (depth 4)",
        ),
        (
            "read_slice",
            json!({"symbol_id": "Foo::bar", "span_kind": "body"}),
            "reading body of `Foo::bar`",
        ),
        (
            "read_slice",
            json!({"path": "src/lib.rs", "start_line": 10, "end_line": 20}),
            "reading `src/lib.rs:10-20`",
        ),
        (
            "grep",
            json!({"pattern": "TODO", "path": "src/"}),
            "grepping for `TODO` in `src/`",
        ),
        (
            "glob",
            json!({"pattern": "**/*.rs"}),
            "globbing for `**/*.rs`",
        ),
        (
            "shell",
            json!({"command": "cargo test\n--workspace"}),
            "running `cargo test --workspace`",
        ),
        (
            "websearch",
            json!({"query": "rust async drop"}),
            "searching the web for `rust async drop`",
        ),
    ];
    for (name, args, expected) in cases {
        let got = crate::human_label_for_call(name, args);
        assert_eq!(&got, expected, "label for `{name}`");
    }
}

#[test]
fn human_label_falls_back_to_tool_name_when_no_template() {
    let label = crate::human_label_for_call("brand_new_tool", &json!({"x": 1}));
    assert_eq!(label, "brand_new_tool");
}

#[test]
fn prepare_arguments_lookup_advertises_only_hooked_tools() {
    let root = temp_workspace("prepare_arguments_lookup");
    let registry = ToolRegistry::new(&root).expect("registry");
    // shell and verify ship bespoke hooks; every path-bearing tool
    // (read_file, grep, …) gets the shared path-alias hook attached
    // uniformly in build_specs. A tool with neither a bespoke hook nor a
    // top-level `path` argument (e.g. repo_map) leaves the slot empty.
    assert!(
        registry.prepare_arguments_for("read_file").is_some(),
        "read_file should advertise a prepare_arguments hook"
    );
    assert!(
        registry.prepare_arguments_for("shell").is_some(),
        "shell should advertise a prepare_arguments hook"
    );
    assert!(
        registry.prepare_arguments_for("verify").is_some(),
        "verify should advertise a prepare_arguments hook"
    );
    assert!(
        registry.prepare_arguments_for("grep").is_some(),
        "grep takes a `path`, so it shares the uniform path-alias hook"
    );
    assert!(
        registry.prepare_arguments_for("repo_map").is_none(),
        "repo_map has no `path` argument and no bespoke hook"
    );
    assert!(
        registry
            .prepare_arguments_for("definitely_not_a_tool")
            .is_none(),
        "unknown tool names resolve to None"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn prepare_arguments_read_file_hook_normalizes_filepath_aliases() {
    let root = temp_workspace("prepare_arguments_read_file_hook");
    let registry = ToolRegistry::new(&root).expect("registry");
    let hook = registry
        .prepare_arguments_for("read_file")
        .expect("read_file hook");

    // `filepath`, `file_path`, and `file` all promote to `path`.
    for alias in ["filepath", "file_path", "file"] {
        let mut args = json!({ alias: "sample.txt" });
        hook(&mut args).expect("hook ok");
        assert_eq!(
            args,
            json!({ "path": "sample.txt" }),
            "alias `{alias}` should normalize to `path`"
        );
    }

    // Canonical key wins when both are present — alias is dropped.
    let mut args = json!({"path": "good.txt", "filepath": "bad.txt"});
    hook(&mut args).expect("hook ok");
    assert_eq!(args, json!({"path": "good.txt"}));

    // Null placeholder for `path` is treated as missing so an alias can
    // fill the slot without colliding with the canonical key.
    let mut args = json!({"path": Value::Null, "filepath": "sample.txt"});
    hook(&mut args).expect("hook ok");
    assert_eq!(args, json!({"path": "sample.txt"}));

    // Null alias is ignored — we never promote `path = null`.
    let mut args = json!({"filepath": Value::Null});
    hook(&mut args).expect("hook ok");
    assert_eq!(args, json!({}));

    // Non-object arguments pass through unchanged.
    let mut args = json!(42);
    hook(&mut args).expect("hook ok");
    assert_eq!(args, json!(42));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn path_bearing_tools_uniformly_accept_path_aliases() {
    // Audit 6.1: every first-party tool that advertises a top-level `path`
    // argument must fold the `filepath`/`file_path`/`file` spelling drift
    // onto `path` before typed deserialization, so a model that misspells
    // the field gets the same forgiving behavior on write_file/grep/glob/
    // the graph tools that it already gets on read_file — instead of a
    // `deny_unknown_fields` hard-reject on some tools and silent acceptance
    // on others. Locks in the uniform policy attached in `build_specs`.
    let root = temp_workspace("path_alias_uniformity");
    let registry = ToolRegistry::new(&root).expect("registry");

    let mut checked = 0usize;
    for spec in registry.specs().iter() {
        let has_top_level_path = spec
            .parameters
            .properties
            .as_ref()
            .is_some_and(|props| props.contains_key("path"));
        if !has_top_level_path {
            continue;
        }
        let hook = registry
            .prepare_arguments_for(&spec.name)
            .unwrap_or_else(|| {
                panic!(
                    "tool `{}` advertises a top-level `path` argument but has \
                     no prepare hook to normalize path aliases",
                    spec.name
                )
            });
        for alias in ["filepath", "file_path", "file"] {
            let mut args = json!({ alias: "sample.txt" });
            hook(&mut args).expect("hook ok");
            assert_eq!(
                args.get("path").and_then(Value::as_str),
                Some("sample.txt"),
                "tool `{}` did not normalize `{alias}` onto `path`",
                spec.name
            );
        }
        checked += 1;
    }

    assert!(
        checked >= 12,
        "expected the path-alias policy to cover the path-bearing tool set; \
         only {checked} tools were checked"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn prepare_arguments_verify_hook_rehomes_stray_command() {
    let root = temp_workspace("prepare_arguments_verify_hook");
    let registry = ToolRegistry::new(&root).expect("registry");
    let hook = registry
        .prepare_arguments_for("verify")
        .expect("verify hook");

    // A `level` value passed under `command` (the `shell` field name) is
    // re-homed onto `level`; `command` is dropped so `deny_unknown_fields`
    // no longer rejects the call.
    let mut args = json!({"command": "full"});
    hook(&mut args).expect("hook ok");
    assert_eq!(args, json!({"level": "full"}));

    // A `scope` value under `command` lands on `scope`.
    let mut args = json!({"command": "workspace"});
    hook(&mut args).expect("hook ok");
    assert_eq!(args, json!({"scope": "workspace"}));

    // Case/whitespace are normalized to the snake_case enum value.
    let mut args = json!({"command": "  QUICK  "});
    hook(&mut args).expect("hook ok");
    assert_eq!(args, json!({"level": "quick"}));

    // An explicit field wins; the stray `command` is still dropped.
    let mut args = json!({"command": "full", "level": "quick"});
    hook(&mut args).expect("hook ok");
    assert_eq!(args, json!({"level": "quick"}));

    // An unrecognized `command` value is dropped (verify then runs with its
    // own defaults instead of hard-failing on the unknown field).
    let mut args = json!({"command": "git status"});
    hook(&mut args).expect("hook ok");
    assert_eq!(args, json!({}));

    // Well-formed calls are untouched.
    let mut args = json!({"scope": "diff", "level": "full"});
    hook(&mut args).expect("hook ok");
    assert_eq!(args, json!({"scope": "diff", "level": "full"}));

    // Non-object arguments pass through unchanged.
    let mut args = json!("nope");
    hook(&mut args).expect("hook ok");
    assert_eq!(args, json!("nope"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn prepare_arguments_shell_hook_normalizes_command_aliases() {
    let root = temp_workspace("prepare_arguments_shell_hook");
    let registry = ToolRegistry::new(&root).expect("registry");
    let hook = registry.prepare_arguments_for("shell").expect("shell hook");

    for alias in ["cmd", "shell_command", "bash", "bash_command"] {
        let mut args = json!({ alias: "ls -la" });
        hook(&mut args).expect("hook ok");
        assert_eq!(
            args,
            json!({ "command": "ls -la" }),
            "alias `{alias}` should normalize to `command`"
        );
    }

    // Canonical `command` wins over `cmd`.
    let mut args = json!({"command": "echo good", "cmd": "echo bad"});
    hook(&mut args).expect("hook ok");
    assert_eq!(args, json!({"command": "echo good"}));

    // Null `command` placeholder is dropped so the alias can land.
    let mut args = json!({"command": Value::Null, "cmd": "echo recovered"});
    hook(&mut args).expect("hook ok");
    assert_eq!(args, json!({"command": "echo recovered"}));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_file_dispatch_normalizes_filepath_alias_via_hook() {
    let root = temp_workspace("read_file_filepath_alias");
    fs::write(root.join("sample.txt"), "hello world").expect("write sample");
    let registry = ToolRegistry::new(&root).expect("registry");

    // Without the hook, `filepath` would trip `deny_unknown_fields` and
    // surface an "invalid tool arguments" error. With it, dispatch
    // succeeds and the typed `ReadFileArgs` deserialization sees the
    // canonical `path` field.
    let result = registry
        .execute(
            ToolCall {
                call_id: "alias_call".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"filepath": "sample.txt", "limit": 11}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["content"], "1\thello world");
    assert_eq!(result.content["path"], "sample.txt");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_file_dispatch_misspelled_alias_still_fails() {
    // Sanity: only the curated aliases are repaired. An arbitrary
    // misspelling like `pth` is still rejected, which is the behavior we
    // want so the model is forced to learn the canonical field name
    // rather than rely on the hook for arbitrary drift.
    let root = temp_workspace("read_file_bad_alias");
    fs::write(root.join("sample.txt"), "hello").expect("write sample");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "bad_alias".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"pth": "sample.txt"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    let error = result.content["error"]
        .as_str()
        .expect("error message present");
    assert!(
        error.contains("invalid tool arguments"),
        "unexpected error: {error}"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_file_accepts_start_line_end_line_aliases() {
    // P0.1: Haiku frequently addresses `read_file` with the `read_slice`
    // line-window vocabulary (`start_line`/`end_line`). Before this fix the
    // `deny_unknown_fields` schema hard-rejected the call, the model retried
    // identically, and the repeated-failure abort path emitted a zero-char
    // answer. The aliases must now resolve to the correct byte window.
    let root = temp_workspace("read_file_line_alias");
    let body = "line one\nline two\nline three\nline four\nline five\n";
    fs::write(root.join("sample.txt"), body).expect("write sample");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "line_alias".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "sample.txt", "start_line": 2, "end_line": 4}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(
        result.status,
        ToolStatus::Success,
        "start_line/end_line must not trip deny_unknown_fields: {:?}",
        result.content
    );
    assert_eq!(result.content["start_line"], 2);
    let content = result.content["content"].as_str().expect("content string");
    // cat -n format, absolute line numbers, lines 2..=4 inclusive (the
    // closing newline of line 4 is part of the byte window).
    assert_eq!(content, "2\tline two\n3\tline three\n4\tline four\n");
    assert!(
        !content.contains("line one") && !content.contains("line five"),
        "window must not bleed past the requested line range: {content}"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_file_start_line_only_reads_to_end_of_file() {
    // `start_line` without `end_line` reads from that line through EOF, the
    // same open-ended semantics `read_slice` offers.
    let root = temp_workspace("read_file_start_line_only");
    let body = "alpha\nbeta\ngamma\n";
    fs::write(root.join("sample.txt"), body).expect("write sample");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "start_only".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "sample.txt", "start_line": 2}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["start_line"], 2);
    let content = result.content["content"].as_str().expect("content string");
    assert_eq!(content, "2\tbeta\n3\tgamma\n");

    let _ = fs::remove_dir_all(root);
}

/// The always-sent core tool prefix (tool name + description + serialized
/// JSON schema for every first-party spec) is what every request pays for
/// and what the provider prompt cache hashes. This is a byte-regression
/// gate for cost-idea G2 ("slim the cached prefix"): the assembled prefix
/// must stay at or below the recorded baseline, and every tool name plus
/// each tool's required params must still be present so the model can call
/// every tool. Trimming a description is fine; dropping a tool or a
/// required param is not. If an intentional addition raises the size,
/// re-measure and bump `PREFIX_BYTES_BASELINE` deliberately.
#[test]
fn core_tool_prefix_stays_within_byte_baseline() {
    // Recorded after the G2 prefix-slimming pass. Was 25_852 before the
    // pass; keep this monotonically non-increasing for cache stability.
    // 24_593 -> 24_700: deliberate bump for the multi-value `decl_search`
    // attribute filter guidance (`base:A|base:B`). The +43 bytes of prefix
    // buys collapsing an N-base enumeration from N serial calls to one,
    // which on a wide hierarchy saves far more tokens (and per-turn budget)
    // than the prose costs — a strongly net-negative token change.
    // 24_700 -> 24_904: deliberate bump for language-aware inheritance
    // guidance on `decl_search`/`hierarchy` — documenting `iface:<Type>`
    // (implements), Dart `with` mixers as `mixin:<Type>`, and the
    // prefix-free `attribute="<Type>"` form that matches base:/iface:/mixin:
    // at once. The data was already indexed; without these recipes the model
    // queried only `base:` and missed every Dart mixer, then fell back to a
    // single-line regex that drops multi-line `with` clauses. The +204 bytes
    // buys correct one-call retrieval of every extender/implementer/mixer of
    // a type — net-negative on tokens versus the failed-regex retries it
    // replaces.
    // 24_904 -> 25_230: deliberate bump for `decl_search transitive=true`. The
    // graph only records each type's DIRECT base, so a one-shot `base:A` query
    // returns only immediate subtypes; the +326 bytes (description sentence +
    // the `transitive` boolean schema property) buys one-call retrieval of the
    // whole transitive subtype closure, replacing the N follow-up `decl_search`
    // calls a model would otherwise issue to walk a deep hierarchy by hand.
    const PREFIX_BYTES_BASELINE: usize = 25_230;

    // Every first-party spec advertised in the always-core path, paired
    // with the required params the model must still see to call it. Tools
    // with no required params carry an empty slice; the presence check
    // alone guards them.
    let cases: Vec<(ToolSpec, &[&str])> = vec![
        (apply_patch_spec(), &[]),
        (decl_search_spec(), &[]),
        (definition_search_spec(), &[]),
        (diff_context_spec(), &[]),
        (downstream_flow_spec(), &[]),
        (glob_spec(), &["pattern"]),
        (grep_spec(), &["pattern"]),
        (hierarchy_spec(), &[]),
        (notebook_edit_spec(), &["path", "expected_sha256"]),
        (plan_patch_spec(), &["objective"]),
        (read_file_spec(), &["path"]),
        (read_slice_spec(), &[]),
        (read_tool_output_spec(), &[]),
        (reference_search_spec(), &[]),
        (refresh_compiler_facts_spec(), &[]),
        (repo_map_spec(), &[]),
        (write_file_spec(), &["path", "content"]),
        (symbol_context_spec(), &["query"]),
        (upstream_flow_spec(), &[]),
        (verify_spec(), &[]),
        (shell_spec(), &["command", "description"]),
        (webfetch_spec(), &["url"]),
        (websearch_spec(), &["query"]),
        (list_skills_spec(), &[]),
        (load_skill_spec(), &["name"]),
        (notes_remember_spec(), &["kind", "text"]),
        (notes_recall_spec(), &["query"]),
        (observations_spec(), &[]),
    ];

    let mut total = 0usize;
    for (mut spec, required) in cases {
        compact_typed_tool_parameters(&mut spec.parameters);
        let params = serde_json::to_string(&spec.parameters).expect("serialize schema");

        // Every tool keeps a non-empty description so the model knows what
        // it does; trimming must not empty one out.
        assert!(
            !spec.description.trim().is_empty(),
            "{} lost its description",
            spec.name
        );
        // Every required param must still be declared in the schema so the
        // model knows the mandatory arguments.
        for param in required {
            assert!(
                params.contains(&format!("\"{param}\"")),
                "{} schema must still declare required param `{param}`; got {params}",
                spec.name
            );
        }

        total += spec.name.len() + spec.description.len() + params.len();
    }

    assert!(
        total <= PREFIX_BYTES_BASELINE,
        "core tool prefix grew to {total} bytes, above the {PREFIX_BYTES_BASELINE}-byte baseline; \
         trim descriptions/schemas or bump the baseline deliberately",
    );
}

// ---------------------------------------------------------------------------
// Inheritance-enumeration grep augmentation (recall-safe, additive only).
// ---------------------------------------------------------------------------

use crate::file_ops::detect_inheritance_grep;

/// Run a content-mode grep and return its `content` value.
async fn run_grep(registry: &ToolRegistry, pattern: &str) -> Value {
    let result = registry
        .execute(
            ToolCall {
                call_id: "grep_aug".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": pattern}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    result.content
}

#[tokio::test]
async fn grep_augments_wrapped_dart_mixin() {
    // The `with WidgetsBindingObserver` clause is on a CONTINUATION line, so
    // a line-oriented grep for `class \w+.*\bwith\b.*WidgetsBindingObserver`
    // cannot match it — but the semantic graph (tree-sitter, span-based)
    // records it as `mixin:WidgetsBindingObserver`.
    let root = temp_workspace("grep_aug_dart_mixin");
    fs::write(
        root.join("home.dart"),
        "class HomeState extends State<Home>\n    with WidgetsBindingObserver {\n  void f() {}\n}\n",
    )
    .expect("write dart");
    let registry = ToolRegistry::new(&root).expect("registry");

    let content = run_grep(&registry, r"class \w+.*\bwith\b.*WidgetsBindingObserver").await;

    // grep itself misses the wrapped declaration.
    let matches = content["matches"].as_array().expect("matches");
    assert!(
        matches.is_empty(),
        "line grep should miss the continuation-line mixin: {matches:?}"
    );

    // ...but the graph augmentation surfaces it.
    let decls = content["graph_declarations"]
        .as_array()
        .expect("graph_declarations present");
    assert!(
        decls.iter().any(|d| {
            d["name"] == json!("HomeState")
                && d["matched_attribute"] == json!("mixin:WidgetsBindingObserver")
                && d["source"] == json!("semantic_graph")
        }),
        "expected HomeState with mixin:WidgetsBindingObserver, got {decls:?}"
    );
    assert_eq!(
        content["graph_hint"]["tool"],
        json!("decl_search"),
        "{}",
        content["graph_hint"]
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn grep_augments_nested_java_extends() {
    // A nested static class whose `extends TypeAdapter<Foo>` clause WRAPS onto
    // a continuation line — the recall gap: a line-oriented grep tying the
    // class name to `extends TypeAdapter` on one line cannot see it, but the
    // graph records `base:TypeAdapter` regardless of nesting depth or line
    // wrapping.
    let root = temp_workspace("grep_aug_java_nested");
    fs::write(
        root.join("Outer.java"),
        "class Outer {\n  static class Adapter\n      extends TypeAdapter<Foo> {\n    void f() {}\n  }\n}\n",
    )
    .expect("write java");
    let registry = ToolRegistry::new(&root).expect("registry");

    // Grep that ties the class declaration to its supertype on one line —
    // this misses the wrapped clause, the exact recall gap the graph fills.
    let content = run_grep(&registry, r"class \w+ extends TypeAdapter").await;

    let matches = content["matches"].as_array().expect("matches");
    assert!(
        matches.is_empty(),
        "line grep should miss the continuation-line extends: {matches:?}"
    );

    let decls = content["graph_declarations"]
        .as_array()
        .expect("graph_declarations present");
    assert!(
        decls.iter().any(|d| {
            d["name"] == json!("Adapter") && d["matched_attribute"] == json!("base:TypeAdapter")
        }),
        "expected nested Adapter with base:TypeAdapter, got {decls:?}"
    );
    assert_eq!(content["graph_hint"]["tool"], json!("decl_search"));
    assert_eq!(
        content["graph_hint"]["arguments"]["attribute"],
        json!("base:TypeAdapter|mixin:TypeAdapter|iface:TypeAdapter")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn grep_inheritance_no_double_count() {
    // A single-line `extends Base` IS matched by the line grep, so it must
    // NOT be duplicated into graph_declarations (de-dup by path+line).
    let root = temp_workspace("grep_aug_no_double");
    fs::write(
        root.join("A.java"),
        "class Child extends Base {\n  void f() {}\n}\n",
    )
    .expect("write java");
    let registry = ToolRegistry::new(&root).expect("registry");

    let content = run_grep(&registry, "class \\w+ extends Base").await;

    let matches = content["matches"].as_array().expect("matches");
    assert!(
        matches.iter().any(|m| m["line"] == json!(1)),
        "grep should match the single-line declaration: {matches:?}"
    );

    // The declaration grep already found is absent from graph_declarations.
    if let Some(decls) = content.get("graph_declarations").and_then(Value::as_array) {
        assert!(
            !decls.iter().any(|d| d["name"] == json!("Child")),
            "single-line extends already in matches must not be re-listed: {decls:?}"
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn grep_ordinary_text_not_augmented() {
    // Ordinary prose greps must never trigger graph augmentation.
    let root = temp_workspace("grep_aug_ordinary");
    fs::write(
        root.join("notes.txt"),
        "class action plan for the project\nworking with the team daily\n",
    )
    .expect("write notes");
    let registry = ToolRegistry::new(&root).expect("registry");

    for pattern in ["class action plan", "with the team"] {
        let content = run_grep(&registry, pattern).await;
        assert!(
            content.get("graph_declarations").is_none(),
            "`{pattern}` must not produce graph_declarations: {content}"
        );
        assert!(
            content.get("graph_hint").is_none(),
            "`{pattern}` must not produce graph_hint: {content}"
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn grep_matches_unchanged_when_augmented() {
    // The `matches` array must be byte-identical whether or not augmentation
    // fires. We compare an augmentation-TRIGGERING grep against a baseline
    // grep whose pattern matches the EXACT same line(s) but provably cannot
    // trigger detection (`Child extends` has an `extends` operator but no
    // capitalized supertype after it, so `detect_inheritance_grep` returns
    // None). Both run over identical content, so their `matches` must be equal
    // — proving the augmentation path never filtered, reordered, or mutated
    // the model's matches.
    let root = temp_workspace("grep_aug_matches_unchanged");
    fs::write(
        root.join("A.java"),
        "class Child extends Base {\n  void f() {}\n}\n",
    )
    .expect("write java");
    let registry = ToolRegistry::new(&root).expect("registry");

    // Sanity: the baseline pattern truly does NOT trigger augmentation.
    assert!(
        detect_inheritance_grep("Child extends").is_none(),
        "baseline pattern must not be detected as inheritance enumeration"
    );

    let augmented = run_grep(&registry, "class \\w+ extends Base").await;
    let baseline = run_grep(&registry, "Child extends").await;

    // The baseline run produced no augmentation...
    assert!(baseline.get("graph_declarations").is_none());
    assert!(baseline.get("graph_hint").is_none());

    // ...and yet the `matches` arrays are byte-identical: a single-line
    // `extends Base` is matched by BOTH patterns and de-duped out of
    // graph_declarations, so the augmenting run's matches equal the baseline's.
    assert_eq!(
        augmented["matches"], baseline["matches"],
        "augmentation must never filter/reorder/mutate matches"
    );
    assert_eq!(
        augmented["matches"].as_array().map(|m| m.len()),
        Some(1),
        "expected the single-line declaration to be matched: {augmented}"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn grep_cpp_inheritance_emits_reference_hint() {
    // C++ records inheritance as references, not `base:` attributes, so the
    // attribute search returns empty. We must NOT claim completeness — emit
    // only a reference_search hint and augment nothing.
    let root = temp_workspace("grep_aug_cpp");
    fs::write(
        root.join("derived.cpp"),
        "class Derived : public Base {\npublic:\n  void f();\n};\n",
    )
    .expect("write cpp");
    let registry = ToolRegistry::new(&root).expect("registry");

    let content = run_grep(&registry, "class \\w+ : public Base").await;

    assert!(
        content.get("graph_declarations").is_none(),
        "C++ must not claim completeness via graph_declarations: {content}"
    );
    let hint = content
        .get("graph_hint")
        .expect("graph_hint present for c++ fallback");
    assert_eq!(hint["tool"], json!("reference_search"), "{hint}");
    assert_eq!(hint["arguments"]["query"], json!("Base"), "{hint}");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn detect_inheritance_grep_positives() {
    // Dart wrapped-mixin pattern.
    let dart = detect_inheritance_grep(r"class \w+.*\bwith\b.*WidgetsBindingObserver")
        .expect("dart mixin pattern qualifies");
    assert_eq!(dart.base_names, vec!["WidgetsBindingObserver"]);
    assert_eq!(dart.decl_kw, "class");

    // Java nested extends pattern: an explicit `extends <Capitalized>`
    // operator anchors a concrete supertype, so it qualifies even with no
    // decl keyword (the graph `kind` scope defaults to `class`).
    let java = detect_inheritance_grep("extends TypeAdapter").expect("extends Foo qualifies");
    assert_eq!(java.base_names, vec!["TypeAdapter"]);
    assert_eq!(java.decl_kw, "class");

    let java2 = detect_inheritance_grep("class \\w+ extends TypeAdapter")
        .expect("class + extends qualifies");
    assert_eq!(java2.base_names, vec!["TypeAdapter"]);
    assert_eq!(java2.decl_kw, "class");

    let impls = detect_inheritance_grep("interface Foo implements Comparable")
        .expect("implements qualifies");
    assert_eq!(impls.base_names, vec!["Comparable"]);
    assert_eq!(impls.decl_kw, "interface");

    // Alternation grep: `extends (A|B|C)` must enumerate subtypes of ALL three
    // bases, not just the last. The old single-`base_name` extractor silently
    // dropped A and B (and thus every subtype found only under them).
    let alt =
        detect_inheritance_grep(r"^export class \w+.*extends (ClientProxy|Server|BaseRpcContext)")
            .expect("alternation extends qualifies");
    assert_eq!(
        alt.base_names,
        vec!["ClientProxy", "Server", "BaseRpcContext"]
    );
    assert_eq!(alt.decl_kw, "class");

    // Ruby `include`/`prepend` mixin idiom — the standard way Ruby classes mix
    // in a module. The graph records `mixin:<leaf>` (plus a qualified
    // `mixin:<ns>:<leaf>`) on the host, so a grep for `include Sidekiq::Component`
    // must seed ONLY the leaf `Component` — never the namespace segment
    // `Sidekiq`, which would inject unrelated `mixin:Sidekiq` declarations and
    // dilute the augment.
    let ruby = detect_inheritance_grep(r"^\s*include\s+Sidekiq::Component\b")
        .expect("ruby include qualifies");
    assert_eq!(ruby.base_names, vec!["Component"]);
    let prepend = detect_inheritance_grep(r"prepend Comparable").expect("ruby prepend qualifies");
    assert!(prepend.base_names.iter().any(|b| b == "Comparable"));
}

#[test]
fn detect_inheritance_grep_negatives() {
    // No supertype literal -> no augmentation.
    assert!(detect_inheritance_grep("class names").is_none());
    // `extends_helper` is one identifier token, not an `extends` operator.
    assert!(detect_inheritance_grep("extends_helper").is_none());
    // Bare `class \w+` has a keyword but no inheritance signal / supertype.
    assert!(detect_inheritance_grep(r"class \w+").is_none());
    // Prose.
    assert!(detect_inheritance_grep("with the team").is_none());
    assert!(detect_inheritance_grep("class action plan").is_none());
    // Diagnostic prose containing the word class but no supertype.
    assert!(detect_inheritance_grep("error: class missing").is_none());
}

#[test]
fn detect_inheritance_grep_extraction() {
    // Generic args stripped.
    let g = detect_inheritance_grep("class X extends TypeAdapter<T>").expect("qualifies");
    assert_eq!(g.base_names, vec!["TypeAdapter"]);

    // Python `class X(Base):` — supertype is the Capitalized name in the
    // first parenthesized group.
    let py = detect_inheritance_grep("class X(Base):").expect("python qualifies");
    assert_eq!(py.base_names, vec!["Base"]);

    // `: Base`-style (Rust/Kotlin/C#) inheritance punctuation.
    let colon = detect_inheritance_grep("struct Foo : Bar").expect("colon qualifies");
    assert_eq!(colon.base_names, vec!["Bar"]);
    assert_eq!(colon.decl_kw, "struct");
}
