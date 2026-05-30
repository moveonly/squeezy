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
        !menu.contains("(session)"),
        "session labels should not be shown in the simplified approval menu: {menu}"
    );

    // Network surfaces the host so users can codify "allow this host"
    // in one keystroke.
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
        git_menu.contains("Deny") && !git_menu.contains("Deny for this session"),
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
    let diff =
        "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-fn old_name() {}\n+fn new_name() {}\n";
    let req = request_with(
        "apply_patch",
        PermissionCapability::Edit,
        "path:src/lib.rs",
        &[("paths", "src/lib.rs"), ("unified_diff", diff)],
    );
    let lines = render_preview(&req);
    let out = flatten(&lines);
    // Sign markers reach the rendered prompt (gutter wiring is live).
    assert!(out.contains("-fn old_name"), "missing remove line: {out}");
    assert!(out.contains("+fn new_name"), "missing add line: {out}");
    // The placeholder text is gone — replaced by the real diff body.
    assert!(
        !out.contains("diff line(s)"),
        "stale 'N diff line(s)' placeholder leaked into preview: {out}"
    );
    // Short patches do not get a "… more lines" tail.
    assert!(
        !out.contains("more lines"),
        "short patch should not be capped: {out}"
    );
}

#[test]
fn edit_preview_caps_long_diff_body_with_summary_tail() {
    // Diff bodies longer than `APPROVAL_DIFF_BODY_CAP` must be
    // truncated and tagged with a "… (N more lines)" summary so the
    // approval prompt stays scannable on short terminals. Reviewers
    // who want the full patch can still run `/diff` after approving.
    //
    // The path uses an unrecognised extension so the cap test does not
    // also exercise the syntax highlighter on every diff line — that
    // path is covered by `edit_preview_renders_unified_diff_with_gutter`
    // and the diff_tests suite. Here the contract under test is purely
    // "cap at N, summarise the rest".
    let mut diff = String::from("--- a/notes.log\n+++ b/notes.log\n@@ -1,60 +1,60 @@\n");
    for i in 0..60 {
        diff.push_str(&format!("-old_line_{i:02}\n"));
        diff.push_str(&format!("+new_line_{i:02}\n"));
    }
    let req = request_with(
        "apply_patch",
        PermissionCapability::Edit,
        "path:notes.log",
        &[("paths", "notes.log"), ("unified_diff", diff.as_str())],
    );
    let lines = render_preview(&req);
    let out = flatten(&lines);
    // Earliest lines render (they're inside the cap window).
    assert!(
        out.contains("+new_line_00"),
        "first add line missing: {out}"
    );
    // A late line is omitted by the cap.
    assert!(
        !out.contains("new_line_59"),
        "diff was not capped — final line leaked through: {out}"
    );
    // The summary tail names the omitted-line count exactly.
    assert!(
        out.contains("more lines)"),
        "cap summary tail missing: {out}"
    );
    let total = 60 * 2; // 60 added + 60 removed
    let expected_more = total - APPROVAL_DIFF_BODY_CAP;
    assert!(
        out.contains(&format!("({expected_more} more lines)")),
        "expected exact summary `({expected_more} more lines)`: {out}"
    );
}

#[test]
fn edit_preview_diff_lines_fallback_when_no_unified_diff() {
    // Older or partial tool emitters may set only `diff_lines` (a count)
    // without the full `unified_diff` blob. The preview keeps that
    // degraded summary so the user still sees an indication of size.
    let req = request_with(
        "apply_patch",
        PermissionCapability::Edit,
        "path:src/lib.rs",
        &[("paths", "src/lib.rs"), ("diff_lines", "12")],
    );
    let out = flatten(&render_preview(&req));
    assert!(
        out.contains("12 diff line(s)"),
        "fallback summary missing: {out}"
    );
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
