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
