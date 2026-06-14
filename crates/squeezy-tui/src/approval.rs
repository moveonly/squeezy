//! Per-capability approval preview blocks.
//!
//! Renders a specialised preview above the decision menu for each tool
//! kind (shell, apply_patch, web, mcp) and shows the proposed rule that
//! "Allow Project" would create.
//!
//! Decision keys: `Y` / `Enter` approve once, `A` / `P` always allow
//! for the project, `N` / `D` deny. The hint row leads with the primary
//! verbs (`Enter` approve once, `A` always allow, `N` deny); `Y`, `P` and
//! `D` stay bound as silent aliases for muscle-memory compatibility. A
//! persistent project-deny ("Never allow … in this repo") sits last in the
//! menu, reachable with the arrow keys + `Enter`.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use squeezy_agent::ToolApprovalRequest;
use squeezy_core::{PermissionCapability, PermissionRequest, PermissionRisk, PermissionRule};

use crate::compact_text;

/// Maximum number of diff lines we surface inline in an approval preview.
/// Anything beyond this is summarised by a "… (N more lines)" tail so the
/// prompt stays scannable on short terminals — reviewers can still see the
/// full patch via `/diff` once the call lands.
const APPROVAL_DIFF_BODY_CAP: usize = 18;
const APPROVAL_CONTEXT_WRAP: usize = 96;

/// The preview block split into its regions, so the renderer can elide the
/// lower-priority rows (rationale, rule) before the command line — and never
/// the decision options — when the terminal is too short to show everything.
pub(crate) struct PreviewParts {
    pub header: Line<'static>,
    /// The `Why: …` rationale (a single `(no rationale provided)` row when the
    /// request carries no context, so the block's shape stays stable).
    pub context: Vec<Line<'static>>,
    /// The capability subject — `$ command`, `✎ path`, diff body, etc.
    pub subject: Vec<Line<'static>>,
    /// The `Rule: …` line the project-scope option would persist.
    pub rule: Vec<Line<'static>>,
}

/// Build the preview block as separate regions. See [`PreviewParts`].
pub(crate) fn render_preview_parts(request: &ToolApprovalRequest) -> PreviewParts {
    let permission = &request.permission;
    let header = header_line(request);
    let mut context = Vec::new();
    let has_rationale = request
        .context
        .as_deref()
        .is_some_and(|ctx| append_context(&mut context, ctx));
    if !has_rationale {
        // Keep the block's shape stable across requests: when no rationale is
        // available (first turn, subagent with no transcript, or a
        // whitespace-only snippet) state the absence rather than dropping the
        // row, so a missing `Why:` reads as "none provided", not "I missed it".
        context.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "Why: ",
                Style::default()
                    .fg(crate::render::theme::quiet())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "(no rationale provided)",
                Style::default().fg(crate::render::theme::quiet()),
            ),
        ]));
    }
    // The permission engine's own verdict rationale (e.g. "pre-classifier
    // requires approval: …" or the AI reviewer's note) is distinct from the
    // assistant's `Why:` transcript snippet; surface it on its own labeled,
    // dimmed row so a real policy reason is visible even when the transcript
    // snippet is empty.
    append_policy_reason(&mut context, &request.reason);
    let mut subject = Vec::new();
    match permission.capability {
        PermissionCapability::Shell => append_shell(&mut subject, permission),
        PermissionCapability::Edit => append_edit(&mut subject, permission),
        PermissionCapability::Read | PermissionCapability::Search => {
            append_read(&mut subject, permission)
        }
        PermissionCapability::Network => append_network(&mut subject, permission),
        PermissionCapability::Mcp => append_mcp(&mut subject, permission, &request.tool_name),
        PermissionCapability::Git
        | PermissionCapability::Compiler
        | PermissionCapability::Destructive => append_generic(&mut subject, permission),
    }
    let mut rule = Vec::new();
    append_rule_preview(&mut rule, permission);
    PreviewParts {
        header,
        context,
        subject,
        rule,
    }
}

/// Render the preview block above the option menu: the header, then a tight
/// `Why → command → Rule` group with no blank lines between them, then a
/// single trailing blank that separates the preview from the decision options.
pub(crate) fn render_preview(request: &ToolApprovalRequest) -> Vec<Line<'static>> {
    let parts = render_preview_parts(request);
    let mut lines =
        Vec::with_capacity(2 + parts.context.len() + parts.subject.len() + parts.rule.len());
    lines.push(parts.header);
    lines.extend(parts.context);
    lines.extend(parts.subject);
    lines.extend(parts.rule);
    lines.push(Line::raw(""));
    lines
}

fn append_context(lines: &mut Vec<Line<'static>>, context: &str) -> bool {
    let trimmed = context.trim();
    if trimmed.is_empty() {
        return false;
    }
    let wrapped = wrap_words(&trimmed.replace('\n', " "), APPROVAL_CONTEXT_WRAP);
    let Some((first, rest)) = wrapped.split_first() else {
        return false;
    };
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "Why: ",
            Style::default()
                .fg(crate::render::theme::quiet())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            first.to_string(),
            Style::default().fg(crate::render::theme::foreground()),
        ),
    ]));
    for line in rest {
        lines.push(Line::from(vec![
            Span::raw("       "),
            Span::styled(
                line.to_string(),
                Style::default().fg(crate::render::theme::foreground()),
            ),
        ]));
    }
    true
}

/// Append a dimmed `Policy: <reason>` block carrying the permission engine's
/// verdict rationale. No-op when the reason is empty/whitespace so a request
/// without a policy note adds no spurious row. Wrapped at the same width as the
/// `Why:` block and labeled distinctly so it never reads as the assistant's own
/// rationale.
fn append_policy_reason(lines: &mut Vec<Line<'static>>, reason: &str) {
    let trimmed = reason.trim();
    if trimmed.is_empty() {
        return;
    }
    let wrapped = wrap_words(&trimmed.replace('\n', " "), APPROVAL_CONTEXT_WRAP);
    let Some((first, rest)) = wrapped.split_first() else {
        return;
    };
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "Policy: ",
            Style::default()
                .fg(crate::render::theme::quiet())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            first.to_string(),
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ]));
    for line in rest {
        lines.push(Line::from(vec![
            Span::raw("          "),
            Span::styled(
                line.to_string(),
                Style::default().fg(crate::render::theme::quiet()),
            ),
        ]));
    }
}

fn header_line(request: &ToolApprovalRequest) -> Line<'static> {
    let permission = &request.permission;
    let capability = permission.capability.as_str();
    // For the shell tool, `tool_name` and `capability` are both "shell"; drop
    // the duplicate so the header reads "Approval needed · shell · high".
    let meta = if request.tool_name == capability {
        format!(" · {capability} · ")
    } else {
        format!(" · {} · {} · ", request.tool_name, capability)
    };
    Line::from(vec![
        Span::styled(
            "Approval needed",
            Style::default()
                .fg(crate::render::theme::blue())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(meta, Style::default().fg(crate::render::theme::quiet())),
        Span::styled(
            permission.risk.as_str().to_string(),
            risk_style(permission.risk),
        ),
    ])
}

/// Colour the risk word on a green → amber → red severity ramp so dangerous
/// commands stand out at a glance. An approval is a rare, deliberate moment,
/// so the amber rung reads as a meaningful caution signal rather than chrome.
fn risk_style(risk: PermissionRisk) -> Style {
    use crate::render::theme;
    match risk {
        PermissionRisk::Low => Style::default().fg(theme::green()),
        PermissionRisk::Medium => Style::default().fg(theme::accent()),
        PermissionRisk::High => Style::default().fg(theme::red()),
        PermissionRisk::Critical => Style::default()
            .fg(theme::red())
            .add_modifier(Modifier::BOLD),
    }
}

fn append_shell(lines: &mut Vec<Line<'static>>, permission: &PermissionRequest) {
    if let Some(command) = permission.metadata.get("command") {
        lines.push(plain_white(format!(
            "$ {}",
            middle_truncate(&command.replace('\n', " "), 160)
        )));
    } else {
        lines.push(plain_white(compact_text(&permission.target, 160)));
    }
    if let Some(cwd) = permission.metadata.get("cwd") {
        lines.push(dim(format!("cwd {cwd}")));
    }
    if let Some(binary) = permission.metadata.get("binary") {
        lines.push(dim(format!("binary {binary}")));
    }
    // Show sandbox posture so the user can see isolation level at approval time.
    let backend = permission
        .metadata
        .get("sandbox_backend")
        .map(String::as_str);
    let mode = permission.metadata.get("sandbox").map(String::as_str);
    let network = permission
        .metadata
        .get("sandbox_network")
        .map(String::as_str);
    let filesystem = permission
        .metadata
        .get("sandbox_filesystem")
        .map(String::as_str);
    if let (Some(b), Some(m)) = (backend, mode)
        && b != "none"
    {
        let mut posture = format!("sandbox {b}  mode {m}");
        if let Some(fs) = filesystem {
            posture.push_str(&format!("  filesystem {fs}"));
        }
        if let Some(net) = network {
            posture.push_str(&format!("  network-policy {net}"));
        }
        lines.push(dim(posture));
    }
    // Show Windows sandbox posture when approval remains the boundary for
    // reads/network or for all filesystem access.
    let windows_posture = permission
        .metadata
        .get("windows_sandbox_posture")
        .map(String::as_str)
        .or_else(|| {
            permission
                .metadata
                .get("windows_no_fs_sandbox")
                .is_some_and(|v| v == "true")
                .then_some("job-object-only")
        });
    if let Some(text) = match windows_posture {
        Some("job-object-only") => {
            Some("Windows: no filesystem/network sandbox; approval is the enforcement boundary")
        }
        Some("restricted-token-writes-only") => {
            Some("Windows: write sandbox only; reads/network are not isolated")
        }
        _ => None,
    } {
        lines.push(warn_line(text.to_string()));
    }
    // On linux-direct-syscalls the seccomp filter blocks AF_UNIX, so
    // `squeezy ask` cannot be used from inside this sandboxed shell child.
    if let Some(hint) = permission.metadata.get("ask_socket_unavailable") {
        lines.push(dim(format!("note: {hint}")));
    }
    // Warn about Windows sandbox posture when the metadata reveals the active
    // tier. The two non-full-isolation cases get distinct messages:
    // - "best_effort_unavailable": Job-Object backend (disabled tier) — no
    //   filesystem or network isolation at all.
    // - "enforced_writes_only": restricted-token tier — filesystem *writes*
    //   are blocked by ACLs, but reads and network are not isolated.
    match permission.metadata.get("filesystem").map(String::as_str) {
        Some("best_effort_unavailable") => {
            lines.push(warn_line(
                "Windows: no filesystem/network isolation; process tree will be killed on timeout/cancel".to_string(),
            ));
        }
        Some("enforced_writes_only") => {
            lines.push(warn_line(
                "Windows: filesystem write isolation enforced; reads and network are not isolated"
                    .to_string(),
            ));
        }
        _ => {}
    }
}

fn append_edit(lines: &mut Vec<Line<'static>>, permission: &PermissionRequest) {
    let paths = permission
        .metadata
        .get("paths")
        .cloned()
        .or_else(|| permission.metadata.get("path").cloned())
        .unwrap_or_else(|| permission.target.clone());
    // Paths arrive newline-delimited so a comma inside a filename is not
    // mistaken for a separator. Filenames cannot contain newlines.
    let path_list: Vec<&str> = paths
        .split('\n')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    for path in path_list.iter().copied().take(4) {
        lines.push(plain_white(format!("✎ {path}")));
    }
    if path_list.len() > 4 {
        lines.push(dim(format!("… (+{} more file(s))", path_list.len() - 4)));
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
            // preview lines (`✎`, `Why:`, `Rule:`).
            line.spans.insert(0, Span::raw("  "));
            lines.push(line);
        }
        if total > shown {
            // Carry the recovery verb so a clipped diff names where the rest
            // lives before the user decides, rather than stopping at a count.
            lines.push(dim(format!(
                "… ({} more lines — full diff via /diff)",
                total - shown
            )));
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
    // A network *shell* command (e.g. `curl https://…`) carries the literal
    // command rather than a structured URL/method; show that instead of
    // rendering the colon-encoded rule target as if it were a URL.
    if let Some(command) = permission.metadata.get("command") {
        lines.push(plain_white(format!(
            "🌐 $ {}",
            middle_truncate(&command.replace('\n', " "), 160)
        )));
    } else {
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
    }
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

/// True when an "Always allow" decision for this request would actually be
/// written to `squeezy.toml`. Mirrors the backend's persistence guard
/// (`permission_rule_for_persistence`): the backend refuses to persist an
/// `Allow` rule on the destructive capability or with an effectively-wildcard
/// target, resolving the call as approve-once instead. The TUI must agree so
/// the project-allow option and its caption never promise a durable rule the
/// backend will silently drop.
pub(crate) fn project_allow_is_persistable(permission: &PermissionRequest) -> bool {
    if permission.capability == PermissionCapability::Destructive {
        return false;
    }
    let rule_capability = permission
        .suggested_rules
        .first()
        .map(|rule| rule.capability.as_str())
        .unwrap_or_else(|| permission.capability.as_str());
    if rule_capability == "destructive" {
        return false;
    }
    let rule_target = permission
        .suggested_rules
        .first()
        .map(|rule| rule.target.as_str())
        .unwrap_or(permission.target.as_str());
    !squeezy_core::target_is_effectively_wildcard(rule_target)
}

fn append_rule_preview(lines: &mut Vec<Line<'static>>, permission: &PermissionRequest) {
    let rule = permission
        .suggested_rules
        .first()
        .map(|rule| format_rule(permission, rule))
        .unwrap_or_else(|| format_rule_target(permission));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "Rule: ",
            Style::default()
                .fg(crate::render::theme::quiet())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            rule,
            Style::default().fg(crate::render::theme::foreground()),
        ),
    ]));
    // Name the actual reach of "Always allow": a persistable rule is written to
    // the project settings file and applies to every future matching request;
    // a non-persistable one (destructive capability or wildcard target) the
    // backend can only honour for this session. Indented under `Rule:` and
    // dimmed so it reads as a caveat.
    let caption = if project_allow_is_persistable(permission) {
        "(saved to squeezy.toml — applies to all matching requests in this project)"
    } else {
        "(session only — this rule cannot be saved to squeezy.toml)"
    };
    lines.push(dim(caption.to_string()));
}

fn format_rule(permission: &PermissionRequest, rule: &PermissionRule) -> String {
    // A network shell command (e.g. `curl https://…`) persists a host-scoped
    // rule; name the host rather than the colon-encoded `shell:cmd:host` target
    // so the rule reads the same way as a web-fetch rule.
    if let Some(label) = network_host_rule_label(permission) {
        return label;
    }
    if permission.tool_name == "shell" || permission.metadata.contains_key("shell_prefix") {
        format!("command prefix {}", rule.target)
    } else {
        format!("{} {}", rule.capability, rule.target)
    }
}

fn format_rule_target(permission: &PermissionRequest) -> String {
    if let Some(label) = network_host_rule_label(permission) {
        return label;
    }
    if permission.tool_name == "shell" || permission.metadata.contains_key("shell_prefix") {
        format!("command prefix {}", permission.target)
    } else {
        format!("{} {}", permission.capability.as_str(), permission.target)
    }
}

/// `network host <host>` for a network-capability request that carries a
/// concrete host, otherwise `None`. Covers both web-fetch and the
/// `shell:cmd:host` shell-network shape so the rule line names the host scope
/// rather than the opaque colon-encoded command target.
fn network_host_rule_label(permission: &PermissionRequest) -> Option<String> {
    if permission.capability != PermissionCapability::Network {
        return None;
    }
    permission
        .metadata
        .get("host")
        .map(|host| format!("network host {host}"))
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

/// A red+bold caution line for "posture that leaves approval as the
/// enforcement boundary" — the genuine-caution tier. Both the Windows
/// sandbox-posture text and the filesystem write-isolation warnings route
/// through here so one underlying fact ("your reads/network aren't isolated")
/// never renders in two severities. Informational posture (backend/mode/
/// network-policy) stays on the dim tier via [`dim`].
fn warn_line(text: String) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            text,
            Style::default()
                .fg(crate::render::theme::red())
                .add_modifier(Modifier::BOLD),
        ),
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

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        if current.is_empty() {
            if word_len <= width {
                current.push_str(word);
            } else {
                push_wrapped_word(&mut lines, word, width);
            }
            continue;
        }

        let current_len = current.chars().count();
        if current_len + 1 + word_len <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            if word_len <= width {
                current.push_str(word);
            } else {
                push_wrapped_word(&mut lines, word, width);
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn push_wrapped_word(lines: &mut Vec<String>, word: &str, width: usize) {
    let width = width.max(1);
    let mut current = String::new();
    for ch in word.chars() {
        if current.chars().count() == width {
            lines.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        lines.push(current);
    }
}

#[cfg(test)]
#[path = "approval_tests.rs"]
mod tests;
