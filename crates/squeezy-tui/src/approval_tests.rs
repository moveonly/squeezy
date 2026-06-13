use super::*;
use crate::format_approval_prompt;
use squeezy_agent::ToolApprovalRequest;
use squeezy_core::{
    PermissionCapability, PermissionMode, PermissionRequest, PermissionRisk, PermissionRule,
    PermissionRuleSource, PermissionScope,
};
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
fn shell_preview_warns_when_filesystem_best_effort_unavailable() {
    // Job-Object-only Windows tier: no filesystem or network isolation. The
    // approval prompt must surface that posture or users approve commands
    // assuming a sandbox that isn't there.
    let req = request_with(
        "shell",
        PermissionCapability::Shell,
        "cargo test",
        &[
            ("command", "cargo test"),
            ("filesystem", "best_effort_unavailable"),
        ],
    );
    let out = flatten(&render_preview(&req));
    assert!(
        out.contains("Windows: no filesystem/network isolation"),
        "best-effort-unavailable warn line missing: {out}"
    );
}

#[test]
fn shell_preview_warns_when_filesystem_enforced_writes_only() {
    // Restricted-token tier: writes are blocked by ACLs, but reads and
    // network are not isolated. Users need that caveat before approving
    // commands that may exfiltrate or hit the network.
    let req = request_with(
        "shell",
        PermissionCapability::Shell,
        "cargo test",
        &[
            ("command", "cargo test"),
            ("filesystem", "enforced_writes_only"),
        ],
    );
    let out = flatten(&render_preview(&req));
    assert!(
        out.contains(
            "Windows: filesystem write isolation enforced; reads and network are not isolated"
        ),
        "enforced-writes-only warn line missing: {out}"
    );
}

#[test]
fn shell_preview_does_not_warn_when_filesystem_enforced() {
    // Fully enforced sandbox (macOS / Linux Landlock / Windows elevated):
    // no Windows-specific posture caveat in the prompt.
    let req = request_with(
        "shell",
        PermissionCapability::Shell,
        "cargo test",
        &[("command", "cargo test"), ("filesystem", "enforced")],
    );
    let out = flatten(&render_preview(&req));
    assert!(
        !out.contains("Windows:"),
        "enforced filesystem must not render a Windows posture warning: {out}"
    );
}

#[test]
fn shell_preview_omits_warn_line_without_filesystem_metadata() {
    // Backwards-compat: pre-existing emitters that don't set the
    // filesystem key keep the legacy preview shape.
    let req = request_with(
        "shell",
        PermissionCapability::Shell,
        "cargo test",
        &[("command", "cargo test")],
    );
    let out = flatten(&render_preview(&req));
    assert!(
        !out.contains("Windows:"),
        "missing filesystem metadata must not render a Windows posture warning: {out}"
    );
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
    assert!(out.contains("Rule: command prefix cargo test"), "{out}");
}

#[test]
fn shell_rule_preview_names_command_prefix_instead_of_internal_capability() {
    let mut req = request_with(
        "shell",
        PermissionCapability::Destructive,
        "find:*",
        &[
            ("command", "find . -name pom.xml"),
            ("shell_prefix", "find:*"),
        ],
    );
    req.permission.suggested_rules.push(PermissionRule::new(
        "destructive",
        "find:*",
        PermissionMode::Allow,
        PermissionRuleSource::Session,
        Some("approved shell command prefix".to_string()),
    ));
    let out = flatten(&render_preview(&req));
    assert!(out.contains("Rule: command prefix find:*"), "{out}");
    assert!(!out.contains("Rule: destructive:find:*"), "{out}");
}

#[test]
fn rule_preview_names_project_wide_persistence() {
    // "Always allow" writes the rule to the project settings file and applies
    // it to every future matching request, so the preview must say so — not
    // leave the durable, project-wide reach to surface only in squeezy.toml.
    let req = request_with(
        "shell",
        PermissionCapability::Shell,
        "cargo test",
        &[("command", "cargo test --workspace")],
    );
    let out = flatten(&render_preview(&req));
    assert!(out.contains("Rule: command prefix cargo test"), "{out}");
    assert!(
        out.contains("saved to squeezy.toml")
            && out.contains("applies to all matching requests in this project"),
        "persistence note missing under Rule line: {out}"
    );
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
    assert!(out.contains("Why: "), "context label missing: {out}");
    assert!(!out.contains("context:"), "old context label leaked: {out}");
    // The context block must appear above the rule preview line so the
    // user reads "why" before scanning the suggested rule and buttons.
    let context_idx = out.find(snippet).expect("context substring");
    let rule_idx = out.find("Rule:").expect("rule preview line missing");
    assert!(
        context_idx < rule_idx,
        "context should render above rule preview ({context_idx} >= {rule_idx})\n{out}",
    );
}

#[test]
fn missing_context_emits_no_rationale_placeholder() {
    // When the request carries no rationale, the block keeps a stable shape:
    // a single `Why: (no rationale provided)` row so the absence is stated,
    // not silently implied by a vanished line.
    let req = request_with(
        "shell",
        PermissionCapability::Shell,
        "ls",
        &[("command", "ls")],
    );
    let out = flatten(&render_preview(&req));
    assert!(!out.contains("context:"), "stray context label: {out}");
    assert!(out.contains("Why: "), "rationale label missing: {out}");
    assert!(
        out.contains("(no rationale provided)"),
        "honest placeholder missing: {out}"
    );
}

#[test]
fn whitespace_context_emits_no_rationale_placeholder() {
    // A whitespace-only snippet is treated as "no rationale" too — the
    // placeholder keeps the layout stable instead of dropping the row.
    let mut req = request_with(
        "shell",
        PermissionCapability::Shell,
        "ls",
        &[("command", "ls")],
    );
    req.context = Some("   \n  ".to_string());
    let out = flatten(&render_preview(&req));
    assert!(
        out.contains("(no rationale provided)"),
        "whitespace context should fall back to the placeholder: {out}"
    );
}

#[test]
fn approval_preview_separates_rationale_command_rule_and_choices() {
    let mut req = request_with(
        "shell",
        PermissionCapability::Shell,
        "cargo test",
        &[("command", "cargo test --workspace"), ("cwd", "/repo")],
    );
    req.context = Some("I need to validate the Rust workspace before reporting back.".to_string());

    let lines = render_preview(&req);
    let rendered: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();

    let why = rendered
        .iter()
        .position(|line| line.contains("Why:"))
        .expect("why line");
    let command = rendered
        .iter()
        .position(|line| line.contains("$ cargo test --workspace"))
        .expect("command line");
    let rule = rendered
        .iter()
        .position(|line| line.contains("Rule: command prefix cargo test"))
        .expect("rule line");

    // The dim persistence note sits directly under the rule line so users see
    // that "Always allow" authors a durable, project-wide rule.
    let persist = rendered
        .iter()
        .position(|line| line.contains("saved to squeezy.toml"))
        .expect("persistence note line");

    assert!(
        why < command && command < rule && rule + 1 == persist,
        "{rendered:#?}"
    );
    // The rationale → command → rule → note group is tight: no blank lines
    // between them (the old loose layout was confusing to read).
    for line in &rendered[why..=persist] {
        assert_ne!(
            line.as_str(),
            "",
            "unexpected blank inside preview: {rendered:#?}"
        );
    }
    // A single trailing blank separates the preview from the decision options.
    assert_eq!(rendered.get(persist + 1).map(String::as_str), Some(""));
}

#[test]
fn approval_preview_header_and_labels_avoid_accent_colors() {
    let mut req = request_with(
        "shell",
        PermissionCapability::Shell,
        "cargo test",
        &[("command", "cargo test")],
    );
    req.context = Some("I need to validate the Rust workspace.".to_string());

    let lines = render_preview(&req);
    let quiet = crate::render::theme::quiet();
    let accent = crate::render::theme::accent();
    let secondary = crate::render::theme::secondary();
    let blue = crate::render::theme::blue();
    let header = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "Approval needed")
        .expect("approval header");
    // The title uses the cool starlight blue for legibility, never the
    // rationed gold (accent/secondary).
    assert_eq!(
        header.style.fg,
        Some(blue),
        "header should use the cool title accent"
    );
    assert_ne!(
        header.style.fg,
        Some(accent),
        "header should not use gold accent"
    );
    assert_ne!(
        header.style.fg,
        Some(secondary),
        "header should not use gold secondary"
    );

    let label_spans = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter(|span| span.content.as_ref() == "Why: " || span.content.as_ref() == "Rule: ");

    for span in label_spans {
        assert_eq!(span.style.fg, Some(quiet), "label should be quiet");
        assert_ne!(span.style.fg, Some(accent), "label should not use accent");
    }
}

#[test]
fn header_dedupes_shell_tool_and_capability() {
    // tool_name "shell" == capability "shell" → collapse to one token so the
    // header reads "Approval needed · shell · …" rather than "· shell · shell".
    let req = request_with(
        "shell",
        PermissionCapability::Shell,
        "cargo test",
        &[("command", "cargo test")],
    );
    let header = flatten(&render_preview(&req))
        .lines()
        .next()
        .expect("header line")
        .to_string();
    assert!(header.contains("Approval needed"), "{header}");
    assert!(!header.contains("shell · shell"), "{header}");
    assert_eq!(header.matches("shell").count(), 1, "{header}");

    // When the tool differs from its capability, both are shown.
    let edit = request_with(
        "apply_patch",
        PermissionCapability::Edit,
        "src/lib.rs",
        &[("path", "src/lib.rs")],
    );
    let edit_header = flatten(&render_preview(&edit))
        .lines()
        .next()
        .expect("header line")
        .to_string();
    assert!(edit_header.contains("apply_patch"), "{edit_header}");
    assert!(edit_header.contains("edit"), "{edit_header}");
}

#[test]
fn header_colors_risk_by_severity() {
    let red = crate::render::theme::red();
    let green = crate::render::theme::green();
    for (risk, expected) in [
        (PermissionRisk::Low, green),
        (PermissionRisk::High, red),
        (PermissionRisk::Critical, red),
    ] {
        let mut req = request_with(
            "shell",
            PermissionCapability::Shell,
            "x",
            &[("command", "x")],
        );
        req.permission.risk = risk;
        let lines = render_preview(&req);
        let risk_span = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == risk.as_str())
            .unwrap_or_else(|| panic!("risk span for {risk:?}"));
        assert_eq!(risk_span.style.fg, Some(expected), "risk {risk:?}");
    }
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
    // truncated and tagged with a "… (N more lines — full diff via /diff)"
    // summary so the approval prompt stays scannable on short terminals and
    // names where the rest lives. Reviewers who want the full patch can still
    // run `/diff` after approving.
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
    // The summary tail names the omitted-line count exactly and points at the
    // recovery verb (`/diff`) so the rest is reachable before deciding.
    assert!(
        out.contains("more lines"),
        "cap summary tail missing: {out}"
    );
    assert!(
        out.contains("full diff via /diff"),
        "cap summary tail should name the /diff recovery: {out}"
    );
    let total = 60 * 2; // 60 added + 60 removed
    let expected_more = total - APPROVAL_DIFF_BODY_CAP;
    assert!(
        out.contains(&format!(
            "({expected_more} more lines — full diff via /diff)"
        )),
        "expected exact summary `({expected_more} more lines — full diff via /diff)`: {out}"
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

#[test]
fn shell_preview_shows_sandbox_posture_when_backend_known() {
    let req = request_with(
        "shell",
        PermissionCapability::Shell,
        "cargo build",
        &[
            ("command", "cargo build"),
            ("sandbox", "required"),
            ("sandbox_network", "deny_by_default"),
            ("sandbox_backend", "linux-direct-syscalls"),
            ("sandbox_filesystem", "enforced"),
        ],
    );
    let out = flatten(&render_preview(&req));
    assert!(
        out.contains("linux-direct-syscalls"),
        "sandbox backend should appear in preview: {out}"
    );
    assert!(
        out.contains("required"),
        "sandbox mode should appear in preview: {out}"
    );
    assert!(
        out.contains("enforced"),
        "sandbox filesystem posture should appear in preview: {out}"
    );
    assert!(
        out.contains("deny_by_default"),
        "sandbox network policy should appear in preview: {out}"
    );
}

#[test]
fn shell_preview_shows_ask_socket_unavailable_hint_for_linux() {
    let hint = "squeezy ask is unavailable inside this shell child because the seccomp profile blocks AF_UNIX socket(2)";
    let req = request_with(
        "shell",
        PermissionCapability::Shell,
        "make test",
        &[
            ("command", "make test"),
            ("sandbox_backend", "linux-direct-syscalls"),
            ("ask_socket_unavailable", hint),
        ],
    );
    let out = flatten(&render_preview(&req));
    assert!(
        out.contains("AF_UNIX"),
        "ask socket hint should appear in preview: {out}"
    );
    assert!(
        out.contains("seccomp"),
        "ask socket hint should mention seccomp: {out}"
    );
}

#[test]
fn shell_preview_omits_sandbox_row_when_backend_is_none() {
    let req = request_with(
        "shell",
        PermissionCapability::Shell,
        "ls",
        &[
            ("command", "ls"),
            ("sandbox", "off"),
            ("sandbox_backend", "none"),
        ],
    );
    let out = flatten(&render_preview(&req));
    assert!(
        !out.contains("sandbox none"),
        "backend=none must not emit a sandbox posture row: {out}"
    );
}
