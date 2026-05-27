use super::*;
use crate::format_approval_prompt;
use squeezy_agent::ToolApprovalRequest;
use squeezy_core::{PermissionCapability, PermissionRequest, PermissionRisk, PermissionScope};
use std::collections::BTreeMap;

fn flatten(lines: &[Line<'static>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn request_with(
    tool_name: &str,
    capability: PermissionCapability,
    target: &str,
    metadata: &[(&str, &str)],
) -> ToolApprovalRequest {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in metadata {
        map.insert(k.to_string(), v.to_string());
    }
    ToolApprovalRequest {
        id: 1,
        call_id: "call-1".to_string(),
        tool_name: tool_name.to_string(),
        scope: PermissionScope::Shell,
        permission: PermissionRequest {
            call_id: "call-1".to_string(),
            tool_name: tool_name.to_string(),
            capability,
            target: target.to_string(),
            risk: PermissionRisk::Medium,
            summary: format!("{tool_name} approval"),
            metadata: map,
            suggested_rules: Vec::new(),
        },
        matched_rule: None,
        reason: "test".to_string(),
        context: None,
        preview: Vec::new(),
    }
}

#[test]
fn shell_preview_shows_command_and_cwd() {
    let req = request_with(
        "shell",
        PermissionCapability::Shell,
        "cargo test",
        &[("command", "cargo test --workspace"), ("cwd", "/repo")],
    );
    let out = flatten(&render_preview(&req));
    assert!(out.contains("cargo test --workspace"), "{out}");
    assert!(out.contains("cwd /repo"), "{out}");
    assert!(out.contains("Allow Project: shell:cargo test"), "{out}");
}

#[test]
fn edit_preview_lists_paths() {
    let req = request_with(
        "edit",
        PermissionCapability::Edit,
        "crates/squeezy-tui/src/lib.rs",
        &[("paths", "crates/foo/a.rs,crates/foo/b.rs")],
    );
    let out = flatten(&render_preview(&req));
    assert!(out.contains("✎ crates/foo/a.rs"), "{out}");
    assert!(out.contains("✎ crates/foo/b.rs"), "{out}");
}

#[test]
fn network_preview_shows_method_and_url() {
    let req = request_with(
        "webfetch",
        PermissionCapability::Network,
        "https://example.com",
        &[("method", "POST"), ("url", "https://example.com/api")],
    );
    let out = flatten(&render_preview(&req));
    assert!(out.contains("POST"), "{out}");
    assert!(out.contains("https://example.com/api"), "{out}");
}

#[test]
fn mcp_preview_shows_server_and_tool() {
    let req = request_with(
        "mcp",
        PermissionCapability::Mcp,
        "filesystem/read",
        &[("server", "filesystem"), ("tool", "read")],
    );
    let out = flatten(&render_preview(&req));
    assert!(out.contains("mcp filesystem/read"), "{out}");
}

#[test]
fn context_field_is_rendered_above_rule_preview() {
    let mut req = request_with(
        "shell",
        PermissionCapability::Shell,
        "grep -r ERROR",
        &[("command", "grep -r ERROR logs/")],
    );
    let snippet = "You asked me to inspect logs, so I'm running grep -r ERROR.";
    req.context = Some(snippet.to_string());
    let lines = render_preview(&req);
    let out = flatten(&lines);
    assert!(out.contains(snippet), "context snippet missing: {out}");
    // The context block must appear above the rule preview line so the
    // user reads "why" before scanning the suggested rule and buttons.
    let context_idx = out.find(snippet).expect("context substring");
    let rule_idx = out
        .find("Allow Project:")
        .expect("rule preview line missing");
    assert!(
        context_idx < rule_idx,
        "context should render above rule preview ({context_idx} >= {rule_idx})\n{out}",
    );
}

#[test]
fn missing_context_keeps_existing_preview_layout() {
    let req = request_with(
        "shell",
        PermissionCapability::Shell,
        "ls",
        &[("command", "ls")],
    );
    let out = flatten(&render_preview(&req));
    assert!(!out.contains("context:"), "stray context label: {out}");
}

#[test]
fn previews_distinguish_by_capability() {
    let shell = render_preview(&request_with(
        "shell",
        PermissionCapability::Shell,
        "cargo test",
        &[("command", "cargo test")],
    ));
    let edit = render_preview(&request_with(
        "edit",
        PermissionCapability::Edit,
        "/tmp/a.rs",
        &[("path", "/tmp/a.rs")],
    ));
    let s = flatten(&shell);
    let e = flatten(&edit);
    assert_ne!(s, e, "shell and edit previews must differ");
    assert!(s.contains("$ cargo test"), "shell missing prompt: {s}");
    assert!(e.contains("✎ /tmp/a.rs"), "edit missing pen icon: {e}");
}

#[test]
fn approval_menu_labels_name_capability_scope() {
    // Shell with a `binary` metadata key names the binary in the label.
    let shell = request_with(
        "shell",
        PermissionCapability::Shell,
        "cargo:*",
        &[("command", "cargo test --workspace"), ("binary", "cargo")],
    );
    let menu = format_approval_prompt(&shell);
    assert!(
        menu.contains("Always allow command cargo"),
        "shell project label missing binary scope: {menu}"
    );
    assert!(
        menu.contains("Allow command cargo (session)"),
        "shell session label missing binary scope: {menu}"
    );

    // Network surfaces the host so users can codify "allow this host" in one
    // keystroke (audit E-UX-06 cites Codex's `ApplyNetworkPolicyAmendment`).
    let net = request_with(
        "webfetch",
        PermissionCapability::Network,
        "domain:docs.rs",
        &[("host", "docs.rs"), ("url", "https://docs.rs/serde")],
    );
    let net_menu = format_approval_prompt(&net);
    assert!(
        net_menu.contains("Always allow host docs.rs"),
        "network project label missing host: {net_menu}"
    );

    // MCP names server/tool so the resulting rule shape is visible.
    let mcp = request_with(
        "mcp",
        PermissionCapability::Mcp,
        "filesystem/read",
        &[("server", "filesystem"), ("tool", "read")],
    );
    let mcp_menu = format_approval_prompt(&mcp);
    assert!(
        mcp_menu.contains("Always allow MCP tool filesystem/read"),
        "mcp project label missing scope: {mcp_menu}"
    );

    // Edit names the write path so users can save a path-scoped rule.
    let edit = request_with(
        "write_file",
        PermissionCapability::Edit,
        "path:/repo/foo.rs",
        &[("path", "/repo/foo.rs")],
    );
    let edit_menu = format_approval_prompt(&edit);
    assert!(
        edit_menu.contains("Always allow edits to /repo/foo.rs"),
        "edit project label missing path: {edit_menu}"
    );

    // Capabilities without scope metadata fall back to the generic label,
    // which is what the original UI did.
    let git = request_with("git", PermissionCapability::Git, "git:status", &[]);
    let git_menu = format_approval_prompt(&git);
    assert!(
        git_menu.contains("Always approve this command in this repo"),
        "git label should fall back to generic when no scope is available: {git_menu}"
    );

    // Deny options remain capability-agnostic.
    assert!(
        git_menu.contains("Deny") && git_menu.contains("Deny for this session"),
        "deny options missing: {git_menu}"
    );
}

#[test]
fn edit_preview_renders_unified_diff_with_gutter() {
    // Approvals for `apply_patch` / `write_file` ship a `unified_diff`
    // metadata key when the tool registry can synthesise one; the
    // preview must thread that body through the same gutter +
    // syntax-highlighted renderer used by `/diff`. Reviewers approve
    // patches more confidently when sign characters and the diff
    // colouring reach the screen, not just a path list.
    let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old line\n+new line\n";
    let req = request_with(
        "apply_patch",
        PermissionCapability::Edit,
        "path:src/lib.rs",
        &[("paths", "src/lib.rs"), ("unified_diff", diff)],
    );
    let lines = render_preview(&req);
    let out = flatten(&lines);
    assert!(out.contains("-old line"), "missing remove line: {out}");
    assert!(out.contains("+new line"), "missing add line: {out}");
}

#[test]
fn edit_preview_without_diff_keeps_legacy_layout() {
    // When the tool registry cannot synthesise a `unified_diff`
    // (e.g. `checkpoint_undo`, or an `apply_patch` with no patches /
    // operations), the preview must stay as the path-only block —
    // the gutter renderer is opt-in via the metadata key.
    let req = request_with(
        "write_file",
        PermissionCapability::Edit,
        "path:foo.rs",
        &[("path", "foo.rs")],
    );
    let out = flatten(&render_preview(&req));
    assert!(out.contains("✎ foo.rs"), "path missing: {out}");
    assert!(
        !out.contains("+"),
        "stray add gutter in legacy layout: {out}"
    );
    assert!(
        !out.contains("-"),
        "stray remove gutter in legacy layout: {out}"
    );
}
