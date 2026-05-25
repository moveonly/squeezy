use super::*;
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
