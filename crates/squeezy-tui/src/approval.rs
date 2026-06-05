//! Per-capability approval preview blocks.
//!
//! Renders a specialised preview above the decision menu for each tool
//! kind (shell, apply_patch, web, mcp) and shows the proposed rule that
//! "Allow Project" would create.
//!
//! Decision keys: `Y` / `Enter` approve once, `A` / `P` always approve
//! for the project, `N` / `D` deny. The hint row surfaces Y / A / N;
//! P and D are silent aliases kept for muscle-memory compatibility.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use squeezy_agent::ToolApprovalRequest;
use squeezy_core::{PermissionCapability, PermissionRequest, PermissionRule};

use crate::compact_text;

/// Maximum number of diff lines we surface inline in an approval preview.
/// Anything beyond this is summarised by a "… (N more lines)" tail so the
/// prompt stays scannable on short terminals — reviewers can still see the
/// full patch via `/diff` once the call lands.
const APPROVAL_DIFF_BODY_CAP: usize = 18;

/// Render the preview block above the option menu.
pub(crate) fn render_preview(request: &ToolApprovalRequest) -> Vec<Line<'static>> {
    let permission = &request.permission;
    let header = header_line(request);
    let mut lines = vec![header];
    if let Some(ctx) = request.context.as_deref() {
        append_context(&mut lines, ctx);
    }
    match permission.capability {
        PermissionCapability::Shell => append_shell(&mut lines, permission),
        PermissionCapability::Edit => append_edit(&mut lines, permission),
        PermissionCapability::Read | PermissionCapability::Search => {
            append_read(&mut lines, permission)
        }
        PermissionCapability::Network => append_network(&mut lines, permission),
        PermissionCapability::Mcp => append_mcp(&mut lines, permission, &request.tool_name),
        PermissionCapability::Git
        | PermissionCapability::Compiler
        | PermissionCapability::Destructive => append_generic(&mut lines, permission),
    }
    append_rule_preview(&mut lines, permission);
    lines
}

fn append_context(lines: &mut Vec<Line<'static>>, context: &str) {
    let trimmed = context.trim();
    if trimmed.is_empty() {
        return;
    }
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "context: ",
            Style::default()
                .fg(crate::render::theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            trimmed.replace('\n', " "),
            Style::default().fg(crate::render::theme::foreground()),
        ),
    ]));
}

fn header_line(request: &ToolApprovalRequest) -> Line<'static> {
    let permission = &request.permission;
    Line::from(vec![
        Span::styled(
            "Approval needed",
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " · {} · {} · {}",
                request.tool_name,
                permission.capability.as_str(),
                permission.risk.as_str(),
            ),
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ])
}

fn append_shell(lines: &mut Vec<Line<'static>>, permission: &PermissionRequest) {
    if let Some(command) = permission.metadata.get("command") {
        lines.push(plain_white(format!("$ {}", middle_truncate(command, 160))));
    } else {
        lines.push(plain_white(permission.target.clone()));
    }
    if let Some(cwd) = permission.metadata.get("cwd") {
        lines.push(dim(format!("cwd {cwd}")));
    }
    if let Some(binary) = permission.metadata.get("binary") {
        lines.push(dim(format!("binary {binary}")));
    }
}

fn append_edit(lines: &mut Vec<Line<'static>>, permission: &PermissionRequest) {
    let paths = permission
        .metadata
        .get("paths")
        .cloned()
        .or_else(|| permission.metadata.get("path").cloned())
        .unwrap_or_else(|| permission.target.clone());
    let path_list: Vec<&str> = paths
        .split(['\n', ','])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    for path in path_list.iter().copied().take(4) {
        lines.push(plain_white(format!("✎ {path}")));
    }
    if let Some(root) = permission.metadata.get("write_root") {
        lines.push(dim(format!("write root {root}")));
    }
    if let Some(diff) = permission.metadata.get("unified_diff") {
        let hint = path_list
            .first()
            .copied()
            .and_then(crate::render::diff::language_hint_from_path)
            .map(str::to_string);
        let body = crate::render::diff::render_patch_full_lines_cached(diff, hint.as_deref());
        let total = body.len();
        let shown = total.min(APPROVAL_DIFF_BODY_CAP);
        for mut line in body.into_iter().take(shown) {
            // Indent the diff body two spaces so it aligns with the other
            // preview lines (`✎`, `context:`, `Allow Project:`).
            line.spans.insert(0, Span::raw("  "));
            lines.push(line);
        }
        if total > shown {
            lines.push(dim(format!("… ({} more lines)", total - shown)));
        }
    } else if let Some(diff_lines) = permission.metadata.get("diff_lines") {
        // Fallback for tool emitters that only know the line count, not the
        // full unified-diff blob. Newer tools synthesise `unified_diff` and
        // skip this branch.
        lines.push(dim(format!("{diff_lines} diff line(s)")));
    }
}

fn append_read(lines: &mut Vec<Line<'static>>, permission: &PermissionRequest) {
    let path = permission
        .metadata
        .get("path")
        .cloned()
        .unwrap_or_else(|| permission.target.clone());
    lines.push(plain_white(format!("📖 {}", compact_text(&path, 160))));
}

fn append_network(lines: &mut Vec<Line<'static>>, permission: &PermissionRequest) {
    let url = permission
        .metadata
        .get("url")
        .cloned()
        .unwrap_or_else(|| permission.target.clone());
    let method = permission
        .metadata
        .get("method")
        .cloned()
        .unwrap_or_else(|| "GET".to_string());
    lines.push(plain_white(format!(
        "🌐 {} {}",
        method,
        compact_text(&url, 160)
    )));
    if let Some(host) = permission.metadata.get("host") {
        lines.push(dim(format!("host {host}")));
    }
}

fn append_mcp(lines: &mut Vec<Line<'static>>, permission: &PermissionRequest, tool_name: &str) {
    let server = permission
        .metadata
        .get("server")
        .cloned()
        .unwrap_or_else(|| "unknown server".to_string());
    let tool = permission
        .metadata
        .get("tool")
        .cloned()
        .unwrap_or_else(|| tool_name.to_string());
    lines.push(plain_white(format!("⚙ mcp {server}/{tool}")));
    if let Some(args) = permission.metadata.get("args_summary") {
        lines.push(dim(compact_text(args, 160)));
    }
}

fn append_generic(lines: &mut Vec<Line<'static>>, permission: &PermissionRequest) {
    lines.push(plain_white(compact_text(&permission.target, 160)));
}

fn append_rule_preview(lines: &mut Vec<Line<'static>>, permission: &PermissionRequest) {
    let rule = permission
        .suggested_rules
        .first()
        .map(format_rule)
        .unwrap_or_else(|| format!("{}:{}", permission.capability.as_str(), permission.target,));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "Allow Project: ",
            Style::default()
                .fg(crate::render::theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            rule,
            Style::default().fg(crate::render::theme::foreground()),
        ),
    ]));
}

fn format_rule(rule: &PermissionRule) -> String {
    format!("{}:{}", rule.capability, rule.target)
}

fn plain_white(text: String) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            text,
            Style::default().fg(crate::render::theme::foreground()),
        ),
    ])
}

fn dim(text: String) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(text, Style::default().fg(crate::render::theme::quiet())),
    ])
}

fn middle_truncate(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    let half = max_chars.saturating_sub(3) / 2;
    let head_end = if half == 0 {
        0
    } else {
        text.char_indices()
            .nth(half)
            .map(|(idx, _)| idx)
            .unwrap_or(text.len())
    };
    let tail_start = if half == 0 {
        text.len()
    } else {
        text.char_indices()
            .nth(char_count - half)
            .map(|(idx, _)| idx)
            .unwrap_or(text.len())
    };
    let mut out = String::with_capacity(head_end + '…'.len_utf8() + text.len() - tail_start);
    out.push_str(&text[..head_end]);
    out.push('…');
    out.push_str(&text[tail_start..]);
    out
}

#[cfg(test)]
#[path = "approval_tests.rs"]
mod tests;
