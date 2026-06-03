use super::*;
use crate::scenario::SqueezyOverlay;

/// `permission_mode = "allow"` in a scenario overlay propagates to every
/// capability gate — including `read` and `ignored_search`. Read-capability
/// tools like `read_tool_output` (called by the model to recover a spilled
/// tool stdout buffer) rely on the `Allow` verdict to bypass the
/// `ApprovalRequested` round-trip and run directly.
#[test]
fn allow_permission_mode_covers_read_capability_so_read_tool_output_auto_approves() {
    let mut config = AppConfig::from_env();
    // Seed every gate to `Ask` to model a developer settings.toml that
    // defaults Read-capability tools to interactive approval. The
    // overlay must override every gate uniformly so the scenario's
    // declared mode is comprehensive.
    config.permissions.read = PermissionMode::Ask;
    config.permissions.edit = PermissionMode::Ask;
    config.permissions.shell = PermissionMode::Ask;
    config.permissions.ignored_search = PermissionMode::Ask;
    config.permissions.web = PermissionMode::Ask;
    config.permissions.mcp = PermissionMode::Ask;

    let overlay = SqueezyOverlay {
        permission_mode: Some("allow".to_string()),
        ..SqueezyOverlay::default()
    };

    apply_overlay(&mut config, &overlay, Path::new("/tmp")).expect("apply_overlay");

    // Read is the capability `read_tool_output` resolves to in
    // `squeezy_tools`'s permission-request mapping. When the verdict for
    // that capability is `Allow`, the agent's permission pipeline
    // short-circuits to `ApprovalDecision::Approved` and no
    // `ApprovalRequested` event is emitted — i.e. the call proceeds
    // directly without an approval round-trip.
    assert_eq!(config.permissions.read, PermissionMode::Allow);
    assert_eq!(config.permissions.ignored_search, PermissionMode::Allow);
    // The non-Read gates round out the comprehensive set.
    assert_eq!(config.permissions.edit, PermissionMode::Allow);
    assert_eq!(config.permissions.shell, PermissionMode::Allow);
    assert_eq!(config.permissions.web, PermissionMode::Allow);
    assert_eq!(config.permissions.mcp, PermissionMode::Allow);
}

/// Minimal scenario + workspace + options for exercising `build_manifest`
/// in isolation (no provider, no real run).
fn manifest_fixtures() -> (Scenario, RunOptions, crate::workspace::ProvisionedWorkspace) {
    let scenario: Scenario = toml::from_str(
        r#"
            id = "cost-rollup"
            title = "cost rollup"
            [workspace]
            local = "."
        "#,
    )
    .expect("parse minimal scenario");
    let options = RunOptions {
        scenario_path: PathBuf::from("/tmp/cost-rollup.toml"),
        out_root: PathBuf::from("/tmp"),
        run_triage: false,
        emit_github: false,
        gh_repo: None,
        live: false,
    };
    let workspace = crate::workspace::ProvisionedWorkspace {
        path: PathBuf::from("/tmp"),
        source: crate::workspace::WorkspaceSource::Local(PathBuf::from("/tmp")),
        cleanup: None,
    };
    (scenario, options, workspace)
}

/// A run that delegated must report a headline cost that INCLUDES the
/// subagent spend — i.e. >= the parent-only cost, and exactly parent +
/// subagent. This is the measurement-integrity guarantee: delegating
/// languages (python / dart) would otherwise undercount the scoreboard.
#[test]
fn manifest_headline_cost_includes_subagent_spend() {
    let (scenario, options, workspace) = manifest_fixtures();
    let parent = 400_000u64;
    let subagent = 150_000u64;

    let manifest = build_manifest(
        &scenario,
        &options,
        &workspace,
        0,
        0,
        &[],
        ManifestCost {
            total_micro_usd: parent + subagent,
            parent_micro_usd: parent,
            subagent_micro_usd: subagent,
        },
        &[],
        "anthropic",
        "claude",
    );

    let totals = &manifest["totals"];
    // Headline cost is parent + subagent, and never below the parent-only
    // figure.
    assert_eq!(totals["cost_micro_usd"], parent + subagent);
    assert!(totals["cost_micro_usd"].as_u64().unwrap() >= parent);
    // Explicit breakdown is preserved so consumers can recover either side.
    assert_eq!(
        totals["total_cost_with_subagents_micro_usd"],
        parent + subagent
    );
    assert_eq!(totals["parent_cost_micro_usd"], parent);
    assert_eq!(totals["subagent_cost_micro_usd"], subagent);
}

/// A run that never delegated reports a headline equal to the parent
/// cost (the subagent fold is a no-op), so the change is invisible to
/// non-delegating scenarios.
#[test]
fn manifest_headline_cost_equals_parent_when_no_subagent() {
    let (scenario, options, workspace) = manifest_fixtures();
    let parent = 250_000u64;

    let manifest = build_manifest(
        &scenario,
        &options,
        &workspace,
        0,
        0,
        &[],
        ManifestCost {
            total_micro_usd: parent,
            parent_micro_usd: parent,
            subagent_micro_usd: 0,
        },
        &[],
        "anthropic",
        "claude",
    );

    let totals = &manifest["totals"];
    assert_eq!(totals["cost_micro_usd"], parent);
    assert_eq!(totals["parent_cost_micro_usd"], parent);
    assert_eq!(totals["subagent_cost_micro_usd"], 0);
}
