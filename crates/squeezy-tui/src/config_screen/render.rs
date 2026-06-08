use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use squeezy_core::{
    config_schema::{ApplyTier, CONFIG_SECTIONS, FieldSource, FieldValue, SectionId},
    is_builtin_tui_theme_name,
};

use super::RESET_ACTIONS;
use super::{
    ConfigScope, ConfigScreenState, FieldEditor, ModelPickerState, SearchOverlayState,
    SecretEntryState, ThemeEditor, ThemeRow, inheritance_label, picker_matches,
    provider_api_key_env, tier_path,
};
use crate::render::palette::{footer_fg, muted_fg};

const MCP_STATUS_COLUMN_WIDTH: usize = 36;

/// Pretty-print an absolute config path: replace the home directory with `~`
/// so the tab subtitle stays compact, while still surfacing the per-machine
/// project hash for the Local tier so the user can grep `~/.squeezy/projects/`
/// for the exact directory.
///
/// Uses `$HOME` first for Unix compatibility; falls back to `dirs::home_dir()`
/// so Windows paths under `%USERPROFILE%` also shorten correctly.
fn display_path(path: &std::path::Path) -> String {
    let full = path.display().to_string();
    let home_candidate = std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(dirs::home_dir);
    if let Some(home) = home_candidate {
        let home_str = home.display().to_string();
        if !home_str.is_empty()
            && let Some(rest) = full.strip_prefix(&home_str)
        {
            return format!("~{rest}");
        }
    }
    full
}

/// Shrink `s` to at most `max` display columns with a middle ellipsis,
/// keeping both the head and the tail so the user can still recognise the
/// home prefix and the trailing filename. Used to keep the /config tab
/// strip on a single row when long repo paths (worktrees, deep nested
/// project layouts) would otherwise push the rightmost tab off-screen.
fn middle_ellipsize(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    if len <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".chars().take(max).collect();
    }
    // Reserve one column for the ellipsis; split the remainder so the
    // tail wins ties — the basename (e.g. `squeezy.toml`) is the most
    // load-bearing part of a config path.
    let budget = max - 1;
    let tail = budget.div_ceil(2);
    let head = budget - tail;
    let mut out = String::with_capacity(max);
    out.extend(chars.iter().take(head));
    out.push('…');
    out.extend(chars.iter().skip(len - tail));
    out
}

pub(crate) fn render(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    // The full-screen prompt editor takes over the whole config surface.
    if state.prompt_editor.is_some() {
        render_prompt_editor(frame, area, state);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // tab strip
            Constraint::Min(0),    // body
            // Two help rows (primary + secondary chords) + the top
            // border = 3 lines. The single-row footer used to drop
            // Ctrl+R/Ctrl+D/Shift+X off the screen entirely.
            Constraint::Length(3), // footer
        ])
        .split(area);

    render_tabs(frame, chunks[0], state);
    render_body(frame, chunks[1], state);
    render_footer(frame, chunks[2], state);
}

fn render_tabs(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    /// One tab cell: tier label, file subtitle, and a small dot when
    /// the tier file actually exists on disk. The dot mirrors the
    /// `[file present]` / `[no file]` indicator on the Reset section
    /// so the user can tell at a glance which tabs are doing work.
    fn push_tab(
        spans: &mut Vec<Span<'static>>,
        label: &'static str,
        subtitle: String,
        active: bool,
        exists: bool,
    ) {
        // The active tab is identified by the amber dot alone — we used to
        // also stamp an extra "▸ " in front of the label, but the ▸
        // separators between tabs already make that look like "▸ ▸ Repo"
        // when the middle/last tab is active. Dropping the active marker
        // keeps the row aligned and leaves a single clear indicator.
        let label_style = if active {
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(muted_fg())
        };
        let dot = if exists { "●" } else { "○" };
        // Active dot is amber, inactive dots are quiet (grey). File
        // existence is still encoded via ●/○ shape, but the colour
        // dimension is reserved for "this is the tab you're editing".
        let dot_style = if active {
            Style::default()
                .fg(crate::render::theme::accent())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(crate::render::theme::quiet())
        };
        let subtitle_text = if subtitle.is_empty() {
            String::new()
        } else {
            format!(" {subtitle}")
        };
        spans.push(Span::styled(label, label_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(dot, dot_style));
        spans.push(Span::styled(
            subtitle_text,
            Style::default().fg(crate::render::theme::quiet()),
        ));
    }
    let user_exists = std::fs::metadata(&state.sources.user_path_default).is_ok();
    let repo_exists = std::fs::metadata(&state.sources.project_path_default).is_ok();
    let local_exists = std::fs::metadata(&state.sources.repo_path_default).is_ok();

    // Reserve width for the three path subtitles so the rightmost tab
    // ("Local") stays on the row even when a worktree pushes the Repo
    // path well past 100 columns. The Paragraph below renders a single
    // Line and any spans past `area.width` are silently clipped, which
    // hid the Local tab entirely at default eval width=140.
    let user_full = display_path(&state.sources.user_path_default);
    let repo_full = display_path(&state.sources.project_path_default);
    let local_full = display_path(&state.sources.repo_path_default);
    // Fixed (non-subtitle) characters on the row, in display columns:
    //   "  Config  " (10) + " │ " (3)
    //   + 3 × tab chrome: label + " ● " (or " ○ ") prefix on the subtitle —
    //     "User ● x" / "Repo ● x" / "Local ● x", i.e. label_len + 3 each →
    //     4+3 + 4+3 + 5+3 = 22 (subtitle bytes themselves go in `budget`)
    //   + 2 × " ▸ " separators (6)
    //   + Repo " (committed)" suffix (12)
    //   + dirty marker "    (changes applied)" (21) when applicable
    let dirty_suffix_len = if state.dirty { 21 } else { 0 };
    let fixed = 10 + 3 + 22 + 6 + 12 + dirty_suffix_len;
    let total = area.width as usize;
    let budget_for_paths = total.saturating_sub(fixed);
    let (user_sub, repo_sub, local_sub) =
        budget_subtitles(&user_full, &repo_full, &local_full, budget_for_paths);

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(22);
    spans.push(Span::styled(
        "  Config  ",
        Style::default()
            .fg(crate::render::theme::accent())
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        " │ ",
        Style::default().fg(crate::render::theme::quiet()),
    ));
    push_tab(
        &mut spans,
        "User",
        user_sub,
        state.scope == ConfigScope::User,
        user_exists,
    );
    spans.push(Span::styled(
        " ▸ ",
        Style::default().fg(crate::render::theme::blue()),
    ));
    let repo_subtitle = if repo_sub.is_empty() {
        String::new()
    } else if repo_exists {
        format!("{repo_sub} (committed)")
    } else {
        repo_sub
    };
    push_tab(
        &mut spans,
        "Repo",
        repo_subtitle,
        state.scope == ConfigScope::Repo,
        repo_exists,
    );
    spans.push(Span::styled(
        " ▸ ",
        Style::default().fg(crate::render::theme::blue()),
    ));
    push_tab(
        &mut spans,
        "Local",
        local_sub,
        state.scope == ConfigScope::Local,
        local_exists,
    );
    if state.dirty {
        spans.push(Span::styled(
            "    (changes applied)",
            Style::default().fg(crate::render::theme::quiet()),
        ));
    }
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(crate::render::theme::quiet()));
    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

/// Allocate `budget` display columns across the three tab subtitles.
/// Short paths get rendered in full; the remaining width is split
/// equally among the still-oversized ones and each is middle-ellipsized
/// to fit. When `budget` is too small even for stubs (≤ ~12 cols),
/// returns empty subtitles so the tab labels themselves stay visible.
fn budget_subtitles(
    user: &str,
    repo: &str,
    local: &str,
    budget: usize,
) -> (String, String, String) {
    let lens = [
        user.chars().count(),
        repo.chars().count(),
        local.chars().count(),
    ];
    let total: usize = lens.iter().sum();
    if total <= budget {
        return (user.to_string(), repo.to_string(), local.to_string());
    }
    // Per-subtitle minimum that still surfaces a recognisable basename
    // (`…/squeezy.toml` is ~14 chars). Below that, drop the subtitle
    // entirely so the label and dot survive.
    let min_per = 14usize;
    if budget < min_per * 3 {
        return (String::new(), String::new(), String::new());
    }
    // Two-pass allocation: short subtitles get their natural width;
    // the remainder is split evenly among the long ones.
    let mut quotas = [0usize; 3];
    let mut remaining = budget;
    let mut long_idx: Vec<usize> = Vec::new();
    let fair_share = budget / 3;
    for (i, &len) in lens.iter().enumerate() {
        if len <= fair_share {
            quotas[i] = len;
            remaining = remaining.saturating_sub(len);
        } else {
            long_idx.push(i);
        }
    }
    if !long_idx.is_empty() {
        let per_long = remaining / long_idx.len();
        for i in long_idx {
            quotas[i] = per_long;
        }
    }
    let trunc = |s: &str, q: usize| -> String {
        if s.chars().count() <= q {
            s.to_string()
        } else {
            middle_ellipsize(s, q)
        }
    };
    (
        trunc(user, quotas[0]),
        trunc(repo, quotas[1]),
        trunc(local, quotas[2]),
    )
}

fn render_body(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    // The live filter replaces the whole body — a small box on top plus the
    // reduced match list — so the sidebar gives way to a full-width view.
    if let Some(search) = &state.search {
        render_filter(frame, area, search);
        return;
    }
    let sidebar_width = 22u16;
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(sidebar_width),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);
    render_sidebar(frame, chunks[0], state);
    let sep_lines: Vec<Line> = (0..area.height).map(|_| Line::from("│")).collect();
    frame.render_widget(
        Paragraph::new(sep_lines).style(Style::default().fg(crate::render::theme::quiet())),
        chunks[1],
    );
    if state.reset_confirm.is_some() {
        render_reset_confirm(frame, chunks[2], state);
    } else if state.discard_confirm {
        render_discard_confirm(frame, chunks[2], state);
    } else if let Some(entry) = &state.secret_entry {
        render_secret_entry(frame, chunks[2], entry);
    } else if let Some(picker) = &state.picker {
        render_model_picker(frame, chunks[2], picker);
    } else if state.current_section().id == SectionId::Reset {
        render_reset_section(frame, chunks[2], state);
    } else if state.current_section().id == SectionId::Themes {
        render_theme_section(frame, chunks[2], state);
    } else if state.current_section().id == SectionId::McpServers {
        render_mcp_section(frame, chunks[2], state);
    } else {
        render_field_pane(frame, chunks[2], state);
    }
}

fn render_reset_section(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    let section = state.current_section();
    let action = match RESET_ACTIONS.iter().find(|a| a.scope == state.scope) {
        Some(a) => a,
        None => return,
    };
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(6);
    lines.push(Line::from(vec![
        Span::styled(
            section.label,
            Style::default()
                .fg(crate::render::theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            section.description,
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ]));
    lines.push(Line::raw(""));

    let tier_path = tier_path(state, action.scope);
    let exists = std::fs::metadata(&tier_path).is_ok();
    let status = if exists {
        Span::styled(
            "[file present]",
            Style::default().fg(crate::render::theme::green()),
        )
    } else {
        Span::styled(
            "[no file]",
            Style::default().fg(crate::render::theme::quiet()),
        )
    };
    lines.push(Line::from(vec![
        Span::styled("› ", Style::default().fg(crate::render::theme::secondary())),
        Span::styled(
            format!("{:<28}", action.label),
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        status,
    ]));
    lines.push(Line::from(vec![
        Span::raw("    "),
        Span::styled(
            action.detail,
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::raw("    "),
        Span::styled(
            tier_path.display().to_string(),
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ]));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("? ", Style::default().fg(crate::render::theme::quiet())),
        Span::styled(
            "Enter to delete this tier's file (with y/n confirmation). Ctrl+Z restores it.",
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ]));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Render the `/mcp` page: one row per configured server with a live
/// status column, plus a trailing "(a) add new server" row. Keeps the
/// section description inline so the user can see what the page does
/// without flipping back to the help footer.
fn render_mcp_section(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    if let Some(form) = &state.mcp_add {
        render_mcp_add_form(frame, area, state, form);
        return;
    }
    if state.mcp_pending_delete.is_some() {
        render_mcp_delete_confirm(frame, area, state);
        return;
    }
    let section = state.current_section();
    let names = state.mcp_server_names();
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(names.len() + 8);
    lines.push(Line::from(vec![
        Span::styled(
            section.label,
            Style::default()
                .fg(crate::render::theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            section.description,
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ]));
    lines.push(Line::raw(""));

    if let Some(banner) = &state.mcp_last_status_line {
        lines.push(Line::from(vec![Span::styled(
            banner.clone(),
            Style::default().fg(crate::render::theme::secondary()),
        )]));
        lines.push(Line::raw(""));
    }

    if names.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "No MCP servers configured.",
            Style::default().fg(crate::render::theme::quiet()),
        )]));
        lines.push(Line::raw(""));
    } else {
        // Header row. Each column has a fixed width so the
        // status-indicator column stays aligned with the body
        // rows. The leading "  " accounts for the `▸ ` row marker.
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default().fg(crate::render::theme::quiet())),
            Span::styled(
                format!("{:<16}", "NAME"),
                Style::default()
                    .fg(crate::render::theme::quiet())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<10}", "STATE"),
                Style::default()
                    .fg(crate::render::theme::quiet())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<8}", "TRANS"),
                Style::default()
                    .fg(crate::render::theme::quiet())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "{:<width$}",
                    "STATUS / ERROR",
                    width = MCP_STATUS_COLUMN_WIDTH
                ),
                Style::default()
                    .fg(crate::render::theme::quiet())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "ENDPOINT",
                Style::default()
                    .fg(crate::render::theme::quiet())
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        for (row, name) in names.iter().enumerate() {
            let server = match state.mcp_servers.get(name) {
                Some(server) => server,
                None => continue,
            };
            let active = row == state.field_index;
            let marker = if active { "▸ " } else { "  " };
            let state_text = if server.enabled {
                "enabled"
            } else {
                "disabled"
            };
            let status_text = state
                .mcp_status
                .per_server
                .get(name)
                .map(|status| format_mcp_row_status_for_server(server, status))
                .unwrap_or_else(|| "—".to_string());
            let endpoint = server
                .command
                .as_deref()
                .or(server.url.as_deref())
                .unwrap_or("-");
            let row_style = if active {
                Style::default()
                    .fg(crate::render::theme::accent())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            // Status indicator: a colored glyph that telegraphs the
            // server's live state at a glance. The colour overrides
            // the row's accent / default foreground because the
            // semantic is more important than the focus styling —
            // a "failed" row should read red even when focused.
            let (icon, icon_color) = mcp_status_icon(
                server,
                state.mcp_status.per_server.get(name),
                state.mcp_animation_tick,
            );
            lines.push(Line::from(vec![
                Span::styled(marker, row_style),
                Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
                Span::styled(format!("{:<16}", name), row_style),
                Span::styled(format!("{:<10}", state_text), row_style),
                Span::styled(format!("{:<8}", server.transport.as_str()), row_style),
                Span::styled(
                    format!(
                        "{:<width$}",
                        mcp_status_cell(&status_text, MCP_STATUS_COLUMN_WIDTH),
                        width = MCP_STATUS_COLUMN_WIDTH
                    ),
                    row_style,
                ),
                Span::styled(endpoint.to_string(), row_style),
            ]));
        }
    }

    // Trailing "(add new)" row.
    let add_row = names.len();
    let add_active = state.field_index == add_row;
    let add_marker = if add_active { "▸ " } else { "  " };
    let add_style = if add_active {
        Style::default()
            .fg(crate::render::theme::accent())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(crate::render::theme::secondary())
    };
    lines.push(Line::from(vec![Span::styled(
        format!("{add_marker}(a) add new MCP server"),
        add_style,
    )]));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![Span::styled(
        "Enter / Space / e   toggle enabled    r   restart    a   add    d / x   remove",
        Style::default().fg(crate::render::theme::quiet()),
    )]));
    lines.push(Line::from(vec![Span::styled(
        "Shift+e / Shift+a applies the change session-only (no settings.toml write); \
         remove prompts y/s/n (y = persist · s = session-only · n / Esc = cancel)",
        Style::default().fg(crate::render::theme::quiet()),
    )]));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Pick the indicator glyph + colour for an MCP server row on the
/// `/mcp` page.
///
/// The status is read primarily from the registry's per-server
/// snapshot, but a disabled server short-circuits to a grey filled
/// circle regardless of any stale "Ready" / "Failed" entry that
/// might still live in `mcp_status` — disabling tears the session
/// down but discovery's last result lingers in the snapshot until
/// the next refresh.
///
/// For servers that are actively discovering or stopping (the `Starting`
/// state, which also covers an in-flight restart) we blink a plain circle at
/// a deliberately slow cadence so the row reads as pending without becoming
/// visually noisy.
pub(crate) fn mcp_status_icon(
    server: &squeezy_core::McpServerConfig,
    status: Option<&squeezy_tools::McpServerStatus>,
    tick: u64,
) -> (char, ratatui::style::Color) {
    use squeezy_tools::McpServerStatus;
    match status {
        Some(McpServerStatus::Starting) => {
            const FRAME_HOLD_TICKS: u64 = 10;
            let frame = (tick / FRAME_HOLD_TICKS) % 2;
            let icon = if frame == 0 { '○' } else { '●' };
            (icon, crate::render::theme::secondary())
        }
        _ if !server.enabled => ('●', crate::render::theme::muted()),
        Some(McpServerStatus::Ready { cached: false, .. }) => ('●', crate::render::theme::green()),
        Some(McpServerStatus::Ready { cached: true, .. }) => {
            // Cached entries are functionally ready but came from
            // the on-disk cache rather than a fresh discovery —
            // distinguish with cyan so the user can tell the
            // difference when the snapshot is stale.
            ('●', crate::render::theme::cyan())
        }
        Some(McpServerStatus::Stale { .. }) => ('●', crate::render::theme::cyan()),
        Some(McpServerStatus::Failed { .. }) | Some(McpServerStatus::Cancelled) => {
            ('●', crate::render::theme::red())
        }
        None => {
            // The server is enabled but we have not yet received a
            // status snapshot — render an open circle in the muted
            // tone so the row reads as "unknown / pending" rather
            // than "ready" or "failed".
            ('○', crate::render::theme::quiet())
        }
    }
}

fn format_mcp_row_status_for_server(
    server: &squeezy_core::McpServerConfig,
    status: &squeezy_tools::McpServerStatus,
) -> String {
    if matches!(status, squeezy_tools::McpServerStatus::Starting) && !server.enabled {
        "stopping".to_string()
    } else {
        format_mcp_row_status(status)
    }
}

fn format_mcp_row_status(status: &squeezy_tools::McpServerStatus) -> String {
    use squeezy_tools::McpServerStatus;
    match status {
        McpServerStatus::Starting => "starting".to_string(),
        McpServerStatus::Ready {
            tools_count,
            cached,
        } => {
            if *cached {
                format!("ready·cached {tools_count}")
            } else {
                format!("ready {tools_count}")
            }
        }
        McpServerStatus::Stale {
            tools_count,
            outcome,
        } => match outcome {
            squeezy_tools::McpStaleOutcome::Failed { error } => {
                format!("stale: {error} ({tools_count} cached)")
            }
            squeezy_tools::McpStaleOutcome::Cancelled => {
                format!("stale: cancelled ({tools_count} cached)")
            }
        },
        McpServerStatus::Failed { error } => format!("failed: {error}"),
        McpServerStatus::Cancelled => "cancelled".to_string(),
    }
}

fn mcp_status_cell(text: &str, width: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= width {
        return normalized;
    }
    let keep = width.saturating_sub(3);
    let mut out: String = normalized.chars().take(keep).collect();
    if keep < width {
        out.push_str("...");
    }
    out
}

fn render_mcp_add_form(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &ConfigScreenState,
    form: &super::McpAddForm,
) {
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(12);
    lines.push(Line::from(vec![Span::styled(
        "Add MCP server",
        Style::default()
            .fg(crate::render::theme::accent())
            .add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::raw(""));
    if let Some(err) = &form.error {
        lines.push(Line::from(vec![Span::styled(
            format!("error: {err}"),
            Style::default().fg(crate::render::theme::red()),
        )]));
        lines.push(Line::raw(""));
    }

    let rows = [
        ("name", form.name.as_str()),
        ("transport", form.transport.as_str()),
        ("command", form.command.as_str()),
        ("url", form.url.as_str()),
    ];
    for (idx, (label, value)) in rows.iter().enumerate() {
        let active = idx == form.field_index;
        let prefix = if active { "▸ " } else { "  " };
        let val_style = if active {
            Style::default()
                .fg(crate::render::theme::accent())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let value_display = if value.is_empty() {
            "—".to_string()
        } else {
            (*value).to_string()
        };
        lines.push(Line::from(vec![
            Span::raw(prefix),
            Span::styled(
                format!("{:<10}", label),
                Style::default().fg(crate::render::theme::quiet()),
            ),
            Span::styled(value_display, val_style),
        ]));
    }
    lines.push(Line::raw(""));
    let mode_label = if form.session_only {
        "session-only (will not write settings.toml)"
    } else {
        "persisted (writes to active scope's settings.toml)"
    };
    lines.push(Line::from(vec![
        Span::styled("mode: ", Style::default().fg(crate::render::theme::quiet())),
        Span::styled(
            mode_label,
            Style::default().fg(crate::render::theme::secondary()),
        ),
    ]));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![Span::styled(
        "Up/Down move · Space cycles transport · Tab toggles session-only · \
         Enter submits · Esc cancels",
        Style::default().fg(crate::render::theme::quiet()),
    )]));
    let _ = state; // future: show the active scope path here
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_mcp_delete_confirm(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    let name = state.mcp_pending_delete.as_deref().unwrap_or("?");
    let mut lines = Vec::with_capacity(6);
    lines.push(Line::from(vec![Span::styled(
        "Remove MCP server",
        Style::default()
            .fg(crate::render::theme::accent())
            .add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![Span::styled(
        format!("Remove `{name}` from configured MCP servers?"),
        Style::default(),
    )]));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![Span::styled(
        "y → remove + persist to settings.toml · s → session-only · n / Esc → cancel",
        Style::default().fg(crate::render::theme::quiet()),
    )]));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_theme_section(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    let section = state.current_section();
    let active_theme = state.effective.tui.theme.clone();
    let active_snapshot = crate::render::theme::resolve_theme(&state.effective, &active_theme);
    let theme_names = crate::render::theme::available_theme_names(&state.effective);
    let token_rows = crate::render::theme::token_rows();
    let total_rows = theme_names.len() + 1 + token_rows.len();
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(
        total_rows
            .min(area.height as usize)
            .saturating_add(usize::from(state.theme_editor.is_some()) * 4)
            .saturating_add(8),
    );
    lines.push(Line::from(vec![
        Span::styled(
            section.label,
            Style::default()
                .fg(crate::render::theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            section.description,
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            "active ",
            Style::default().fg(crate::render::theme::quiet()),
        ),
        Span::styled(
            active_theme.clone(),
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::raw(""));

    let editing = state.theme_editor.is_some();
    let (row_start, row_end, hidden_above, hidden_below) = if editing {
        (state.field_index, state.field_index + 1, 0, 0)
    } else {
        let detail_rows = 4usize + usize::from(state.theme_editor.is_some()) * 4;
        let row_area = (area.height as usize).saturating_sub(detail_rows);
        let (start, end) = field_row_window(total_rows, state.field_index, row_area);
        (start, end, start, total_rows.saturating_sub(end))
    };

    if hidden_above > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ▲ {hidden_above} more above"),
            Style::default().fg(crate::render::theme::quiet()),
        )));
    }

    for row in row_start..row_end {
        let focused = row == state.field_index;
        let prefix = if focused { "› " } else { "  " };
        let prefix_style = Style::default().fg(if focused {
            crate::render::theme::secondary()
        } else {
            crate::render::theme::quiet()
        });
        let theme_row = if row < theme_names.len() {
            Some(ThemeRow::Theme(theme_names[row].clone()))
        } else if row == theme_names.len() {
            Some(ThemeRow::New)
        } else {
            token_rows
                .get(row.saturating_sub(theme_names.len() + 1))
                .copied()
                .map(ThemeRow::Color)
        };
        match theme_row {
            Some(ThemeRow::Theme(name)) => {
                let snapshot = crate::render::theme::resolve_theme(&state.effective, &name);
                let accent = snapshot
                    .resolve(crate::render::theme::token::PALETTE_ACCENT)
                    .unwrap_or([255, 255, 255]);
                let active = name == active_theme;
                let style = if focused {
                    Style::default()
                        .fg(crate::render::theme::secondary())
                        .add_modifier(Modifier::BOLD)
                } else if active {
                    Style::default()
                        .fg(crate::render::theme::accent())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(muted_fg())
                };
                let badge = if is_builtin_tui_theme_name(&name) {
                    "[builtin]"
                } else {
                    "[custom]"
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, prefix_style),
                    theme_swatch(accent),
                    Span::raw("  "),
                    Span::styled(format!("{:<22}", name), style),
                    Span::styled(
                        if active { "● active " } else { "         " },
                        Style::default().fg(if active {
                            crate::render::theme::green()
                        } else {
                            crate::render::theme::quiet()
                        }),
                    ),
                    Span::styled(badge, Style::default().fg(crate::render::theme::quiet())),
                    Span::raw("  "),
                    Span::styled(
                        if is_builtin_tui_theme_name(&name) {
                            "Enter select"
                        } else {
                            "Enter select · r rename · d delete"
                        },
                        Style::default().fg(crate::render::theme::quiet()),
                    ),
                ]));
            }
            Some(ThemeRow::New) => {
                let style = if focused {
                    Style::default()
                        .fg(crate::render::theme::secondary())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(crate::render::theme::magenta())
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, prefix_style),
                    Span::styled("+", Style::default().fg(crate::render::theme::magenta())),
                    Span::raw("   "),
                    Span::styled(format!("{:<22}", "new theme"), style),
                    Span::styled(
                        "Enter create · n anywhere",
                        Style::default().fg(crate::render::theme::quiet()),
                    ),
                ]));
            }
            Some(ThemeRow::Color(token)) => {
                let rgb = active_snapshot.resolve(token).unwrap_or([255, 255, 255]);
                let overridden = state
                    .effective
                    .tui
                    .themes
                    .get(&active_theme)
                    .is_some_and(|theme| theme.colors.contains_key(token));
                let token_style = if focused {
                    Style::default()
                        .fg(crate::render::theme::secondary())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(muted_fg())
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, prefix_style),
                    theme_swatch(rgb),
                    Span::raw("  "),
                    Span::styled(
                        format!("{:<13}", crate::render::theme::token_category(token)),
                        Style::default().fg(crate::render::theme::quiet()),
                    ),
                    Span::styled(format!("{:<28}", token), token_style),
                    Span::styled(format_rgb(rgb), Style::default().fg(muted_fg())),
                    Span::raw(" "),
                    Span::styled(
                        if overridden { "[override]" } else { "" },
                        Style::default().fg(crate::render::theme::magenta()),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        "Enter edit RGB",
                        Style::default().fg(crate::render::theme::quiet()),
                    ),
                ]));
            }
            None => {}
        }
    }

    if hidden_below > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ▼ {hidden_below} more below"),
            Style::default().fg(crate::render::theme::quiet()),
        )));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Theme rows: Enter select · r rename custom · d delete custom · n new",
        Style::default().fg(crate::render::theme::quiet()),
    )));
    lines.push(Line::from(Span::styled(
        "Color rows: Enter edit RGB · Ctrl+R clear selected override · Ctrl+Z undo last write",
        Style::default().fg(crate::render::theme::quiet()),
    )));

    if let Some(editor) = &state.theme_editor {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![Span::styled(
            "── editing ──",
            Style::default().fg(crate::render::theme::accent()),
        )]));
        lines.extend(render_theme_editor_lines(editor));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn theme_swatch(rgb: [u8; 3]) -> Span<'static> {
    Span::styled(
        "██",
        Style::default().fg(crate::render::palette::best_color((rgb[0], rgb[1], rgb[2]))),
    )
}

fn format_rgb(rgb: [u8; 3]) -> String {
    format!("[{:>3}, {:>3}, {:>3}]", rgb[0], rgb[1], rgb[2])
}

fn render_theme_editor_lines(editor: &ThemeEditor) -> Vec<Line<'static>> {
    match editor {
        ThemeEditor::Name { draft, cursor } => {
            vec![
                caret_line(draft, *cursor),
                Line::from(Span::styled(
                    "Enter to create from the active theme · Esc to cancel",
                    Style::default().fg(crate::render::theme::quiet()),
                )),
            ]
        }
        ThemeEditor::Rename {
            original,
            draft,
            cursor,
        } => {
            vec![
                Line::from(vec![
                    Span::styled(
                        "rename ",
                        Style::default().fg(crate::render::theme::quiet()),
                    ),
                    Span::styled(
                        original.clone(),
                        Style::default().fg(crate::render::theme::secondary()),
                    ),
                ]),
                caret_line(draft, *cursor),
                Line::from(Span::styled(
                    "Enter to rename · Esc to cancel",
                    Style::default().fg(crate::render::theme::quiet()),
                )),
            ]
        }
        ThemeEditor::Rgb {
            theme,
            token,
            draft,
            cursor,
        } => {
            vec![
                Line::from(vec![
                    Span::styled("theme ", Style::default().fg(crate::render::theme::quiet())),
                    Span::styled(
                        theme.clone(),
                        Style::default().fg(crate::render::theme::secondary()),
                    ),
                    Span::styled(
                        "  token ",
                        Style::default().fg(crate::render::theme::quiet()),
                    ),
                    Span::styled(
                        *token,
                        Style::default().fg(crate::render::theme::secondary()),
                    ),
                ]),
                caret_line(draft, *cursor),
                Line::from(Span::styled(
                    "RGB as r,g,b · Enter to commit · Esc to cancel",
                    Style::default().fg(crate::render::theme::quiet()),
                )),
            ]
        }
    }
}

fn render_reset_confirm(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    let scope = state.reset_confirm.expect("guarded by caller");
    let path = tier_path(state, scope);
    let exists = std::fs::metadata(&path).is_ok();
    let preview = if exists {
        state.reset_preview(scope)
    } else {
        Vec::new()
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        "Reset confirmation",
        Style::default()
            .fg(crate::render::theme::accent())
            .add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw("  Delete the "),
        Span::styled(
            scope.label(),
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" settings file?"),
    ]));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(
            "    path   ",
            Style::default().fg(crate::render::theme::quiet()),
        ),
        Span::raw(path.display().to_string()),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            "    status ",
            Style::default().fg(crate::render::theme::quiet()),
        ),
        Span::styled(
            if exists { "exists" } else { "(no file)" },
            Style::default().fg(if exists {
                crate::render::theme::green()
            } else {
                crate::render::theme::quiet()
            }),
        ),
    ]));
    lines.push(Line::raw(""));

    if !exists {
        lines.push(Line::from(Span::styled(
            "  Nothing to delete — that tier file does not exist on disk. \
             Confirming is harmless: the effective config doesn't change.",
            Style::default().fg(crate::render::theme::quiet()),
        )));
    } else if preview.is_empty() {
        lines.push(Line::from(Span::styled(
            "  The file exists, but every key in it matches the value that \
             would still be effective after deletion (env override, identical \
             higher-priority tier value, or the binary default). \
             Confirming deletes the file without changing any displayed value.",
            Style::default().fg(crate::render::theme::quiet()),
        )));
    } else {
        let plural = if preview.len() == 1 { "" } else { "s" };
        lines.push(Line::from(vec![Span::styled(
            format!(
                "  {} key{plural} will change effective value:",
                preview.len()
            ),
            Style::default().fg(crate::render::theme::accent()),
        )]));
        lines.push(Line::raw(""));
        // Cap the list to a reasonable height so the confirm overlay doesn't
        // overflow the pane on small terminals; a count of "and N more"
        // keeps the user informed without scrolling.
        let max_rows = 12usize;
        for entry in preview.iter().take(max_rows) {
            let after_label = inheritance_label(scope, entry.after_source);
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    format!("{}.{}", entry.section_label, entry.field_label),
                    Style::default().fg(muted_fg()),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::raw("       "),
                Span::styled(
                    entry.before.clone(),
                    Style::default().fg(crate::render::theme::secondary()),
                ),
                Span::raw("  →  "),
                Span::styled(
                    entry.after.clone(),
                    Style::default().fg(crate::render::theme::green()),
                ),
                Span::raw(" "),
                Span::styled(after_label, source_style(entry.after_source)),
            ]));
        }
        if preview.len() > max_rows {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                format!("    … and {} more", preview.len() - max_rows),
                Style::default().fg(crate::render::theme::quiet()),
            )));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![Span::styled(
        "  Other tabs are not touched. Ctrl+Z restores the deleted file.",
        Style::default().fg(crate::render::theme::quiet()),
    )]));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(
            "y",
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " delete   ",
            Style::default().fg(crate::render::theme::quiet()),
        ),
        Span::styled(
            "n",
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " cancel   ",
            Style::default().fg(crate::render::theme::quiet()),
        ),
        Span::styled(
            "Esc",
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " cancel",
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ]));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_discard_confirm(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        "Discard all session writes",
        Style::default()
            .fg(crate::render::theme::accent())
            .add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::raw(""));
    let count = state.undo_stack.len();
    let plural = if count == 1 { "" } else { "s" };
    lines.push(Line::from(vec![
        Span::raw("  Revert "),
        Span::styled(
            format!("{count} write{plural}"),
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" made since the screen opened?"),
    ]));
    lines.push(Line::raw(""));
    // Affected tier files — same baseline list that `discard_all` walks.
    for (path, _) in &state.baseline {
        if path.as_os_str().is_empty() {
            continue;
        }
        lines.push(Line::from(vec![
            Span::styled(
                "    file  ",
                Style::default().fg(crate::render::theme::quiet()),
            ),
            Span::raw(path.display().to_string()),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  Each tier file is restored to the bytes captured when /config opened.",
        Style::default().fg(crate::render::theme::quiet()),
    )));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(
            "y",
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " discard   ",
            Style::default().fg(crate::render::theme::quiet()),
        ),
        Span::styled(
            "n",
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " cancel   ",
            Style::default().fg(crate::render::theme::quiet()),
        ),
        Span::styled(
            "Esc",
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " cancel",
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ]));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Render the secret-entry value line with a caret at char-index `cursor`.
/// `display` is the already-masked-or-revealed string, whose chars stay 1:1
/// with `draft`, so the draft cursor indexes it directly. The caret reverses
/// the glyph it sits on, or an accent underscore when parked past the end.
fn secret_caret_line(display: &str, cursor: usize) -> Line<'static> {
    let chars: Vec<char> = display.chars().collect();
    let cursor = cursor.min(chars.len());
    let before: String = chars[..cursor].iter().collect();
    let after: String = chars[cursor..].iter().skip(1).collect();
    let mut spans = vec![
        Span::styled("  ", Style::default()),
        Span::styled(before, Style::default().fg(muted_fg())),
    ];
    match chars.get(cursor) {
        Some(c) => spans.push(Span::styled(
            c.to_string(),
            Style::default()
                .fg(muted_fg())
                .add_modifier(Modifier::REVERSED),
        )),
        None => spans.push(Span::styled(
            "_",
            Style::default().fg(crate::render::theme::accent()),
        )),
    }
    spans.push(Span::styled(after, Style::default().fg(muted_fg())));
    Line::from(spans)
}

fn render_secret_entry(frame: &mut Frame<'_>, area: Rect, entry: &SecretEntryState) {
    let display: String = if entry.reveal {
        // F2 toggle — show the full plaintext for verification.
        entry.draft.clone()
    } else {
        "•".repeat(entry.char_len())
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(
                "Set API key",
                Style::default()
                    .fg(crate::render::theme::accent())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!("for {}", entry.provider_label),
                Style::default().fg(crate::render::theme::quiet()),
            ),
        ]),
        Line::from(vec![
            Span::styled("env  ", Style::default().fg(crate::render::theme::quiet())),
            Span::styled(entry.env_var.as_str(), Style::default().fg(muted_fg())),
        ]),
        Line::raw(""),
        secret_caret_line(&display, entry.cursor),
        Line::raw(""),
        Line::from(vec![Span::styled(
            "Paste your key. Saved as inline `api_key` in the active scope's settings \
             TOML (User or Local). Refuses the committed repo TOML. The running \
             provider client is rebuilt on the next prompt.",
            Style::default().fg(crate::render::theme::quiet()),
        )]),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                "Enter ",
                Style::default().fg(crate::render::theme::secondary()),
            ),
            Span::styled("save  ", Style::default().fg(crate::render::theme::quiet())),
            Span::styled(
                "F2 ",
                Style::default().fg(crate::render::theme::secondary()),
            ),
            Span::styled(
                if entry.reveal {
                    "hide key  "
                } else {
                    "reveal full key  "
                },
                Style::default().fg(crate::render::theme::quiet()),
            ),
            Span::styled(
                "Esc ",
                Style::default().fg(crate::render::theme::secondary()),
            ),
            Span::styled("cancel", Style::default().fg(crate::render::theme::quiet())),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Render the live filter: a small box on top showing the query, and below it
/// the reduced, ranked, highlighted match list. Replaces the whole body while
/// the filter is open.
fn render_filter(frame: &mut Frame<'_>, area: Rect, search: &SearchOverlayState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);
    render_filter_box(frame, chunks[0], search);
    render_filter_list(frame, chunks[1], search);
}

fn render_filter_box(frame: &mut Frame<'_>, area: Rect, search: &SearchOverlayState) {
    let below_threshold = search.query.chars().count() < super::FILTER_MIN_QUERY;
    let mut spans = vec![
        Span::styled(
            "⌕ ",
            Style::default()
                .fg(crate::render::theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(search.query.clone()),
        Span::styled("▏", Style::default().fg(crate::render::theme::accent())),
    ];
    let tail = if below_threshold {
        "   keep typing to filter…".to_string()
    } else {
        let n = search.matches.len();
        format!("   {n} match{}", if n == 1 { "" } else { "es" })
    };
    spans.push(Span::styled(
        tail,
        Style::default().fg(crate::render::theme::quiet()),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(crate::render::theme::quiet()))
        .title(Span::styled(
            " Filter settings ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

fn render_filter_list(frame: &mut Frame<'_>, area: Rect, search: &SearchOverlayState) {
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(search.matches.len() + 3);
    if search.matches.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matches",
            Style::default().fg(crate::render::theme::quiet()),
        )));
    } else {
        // Reserve a blank + help row at the bottom; scroll the window around
        // the cursor so navigating past the visible region keeps the
        // highlighted row on-screen (mirrors the model picker).
        let chrome = 2u16;
        let visible = area.height.saturating_sub(chrome).max(1) as usize;
        let total = search.matches.len();
        let cursor = search.cursor.min(total - 1);
        let start = if total <= visible {
            0
        } else if cursor + 1 > visible {
            (cursor + 1 - visible).min(total - visible)
        } else {
            0
        };
        let end = (start + visible).min(total);
        if start > 0 {
            lines.push(Line::from(Span::styled(
                format!("  ▲ {start} more above"),
                Style::default().fg(crate::render::theme::quiet()),
            )));
        }
        for (idx, m) in search.matches[start..end].iter().enumerate() {
            lines.push(filter_match_line(&search.query, *m, start + idx == cursor));
        }
        if end < total {
            lines.push(Line::from(Span::styled(
                format!("  ▼ {} more below", total - end),
                Style::default().fg(crate::render::theme::quiet()),
            )));
        }
    }
    // Controls live in the (filter-aware) footer, so the list needs no help row.
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// One result row: `Section › Option` (or just the section name for a
/// field-less section), with the query's matched characters highlighted.
fn filter_match_line(query: &str, m: super::SearchMatch, active: bool) -> Line<'static> {
    let section = &CONFIG_SECTIONS[m.section_index];
    let prefix = if active { "› " } else { "  " };
    let prefix_style = Style::default().fg(if active {
        crate::render::theme::secondary()
    } else {
        crate::render::theme::quiet()
    });
    let base = if active {
        Style::default()
            .fg(crate::render::theme::secondary())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(muted_fg())
    };
    let hit = Style::default()
        .fg(crate::render::theme::green())
        .add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::styled(prefix, prefix_style)];
    // Highlighting is computed per column (section name, then option name). The
    // match was scored over the joined "<section> <option>" string plus the
    // option's value/description, so a query that matched the value, the
    // description, or straddles the column boundary may highlight nothing — the
    // row still appears, just without emphasis. Acceptable: it under-highlights,
    // never mis-highlights.
    let option_label: Option<&str> = match m.target {
        super::SearchTarget::Field(fidx) => Some(section.fields[fidx].label),
        super::SearchTarget::SyntheticApiKey => Some("api_key"),
        super::SearchTarget::Section => None,
    };
    match option_label {
        Some(label) => {
            // Section column stays dim (it's context), but still highlight a
            // query that landed on the section name rather than the option.
            let quiet = Style::default().fg(crate::render::theme::quiet());
            spans.extend(highlight_spans(section.label, query, quiet, hit));
            let pad = 22usize.saturating_sub(section.label.chars().count()).max(1);
            spans.push(Span::raw(" ".repeat(pad)));
            spans.extend(highlight_spans(label, query, base, hit));
        }
        None => {
            spans.extend(highlight_spans(section.label, query, base, hit));
            spans.push(Span::styled(
                "  · panel",
                Style::default().fg(crate::render::theme::quiet()),
            ));
        }
    }
    Line::from(spans)
}

/// Split `text` into spans, styling the characters that `query` matches (as a
/// subsequence) with `hit` and the rest with `base`. Falls back to a single
/// `base` span when `query` isn't a subsequence of `text` (e.g. the row was
/// matched on its help text, not its label).
fn highlight_spans(text: &str, query: &str, base: Style, hit: Style) -> Vec<Span<'static>> {
    let positions = match super::subsequence_match_positions(text, query) {
        Some(p) if !p.is_empty() => p,
        _ => return vec![Span::styled(text.to_string(), base)],
    };
    let hits: std::collections::HashSet<usize> = positions.into_iter().collect();
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut buf_hit = false;
    for (idx, ch) in text.chars().enumerate() {
        let is_hit = hits.contains(&idx);
        if !buf.is_empty() && is_hit != buf_hit {
            spans.push(Span::styled(
                std::mem::take(&mut buf),
                if buf_hit { hit } else { base },
            ));
        }
        buf_hit = is_hit;
        buf.push(ch);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, if buf_hit { hit } else { base }));
    }
    spans
}

fn render_model_picker(frame: &mut Frame<'_>, area: Rect, picker: &ModelPickerState) {
    let matches = picker_matches(picker);
    let scope_label = if picker.all_providers {
        "all providers"
    } else {
        picker.current_provider
    };
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(matches.len() + 4);
    lines.push(Line::from(vec![
        Span::styled(
            "Pick model",
            Style::default()
                .fg(crate::render::theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("scope: {scope_label}"),
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            "filter ",
            Style::default().fg(crate::render::theme::quiet()),
        ),
        Span::raw("› "),
        Span::raw(picker.filter.clone()),
        Span::styled("_", Style::default().fg(crate::render::theme::accent())),
    ]));
    lines.push(Line::raw(""));
    if matches.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matches · Enter to commit the filter as a custom model id",
            Style::default().fg(crate::render::theme::quiet()),
        )));
    } else {
        // Same scrolling treatment as the search overlay — large
        // registries (the all-providers tab can be 60+ entries) used to
        // run the cursor straight off the bottom of the pane.
        let chrome = 5u16;
        let visible = area.height.saturating_sub(chrome).max(1) as usize;
        let total = matches.len();
        let cursor = picker.cursor.min(total - 1);
        let start = if total <= visible {
            0
        } else if cursor + 1 > visible {
            (cursor + 1 - visible).min(total - visible)
        } else {
            0
        };
        let end = (start + visible).min(total);
        if start > 0 {
            lines.push(Line::from(Span::styled(
                format!("  ▲ {} more above", start),
                Style::default().fg(crate::render::theme::quiet()),
            )));
        }
        for (idx, info) in matches[start..end].iter().enumerate() {
            let row_idx = start + idx;
            let active = row_idx == cursor;
            let prefix = if active { "› " } else { "  " };
            let style = if active {
                Style::default()
                    .fg(crate::render::theme::secondary())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(muted_fg())
            };
            let mut row = vec![
                Span::styled(
                    prefix,
                    Style::default().fg(if active {
                        crate::render::theme::secondary()
                    } else {
                        crate::render::theme::quiet()
                    }),
                ),
                Span::styled(format!("{:<32}", info.id), style),
            ];
            if picker.all_providers {
                row.push(Span::styled(
                    format!("{:<12}", info.provider),
                    Style::default().fg(crate::render::theme::quiet()),
                ));
            }
            for (tag, present) in [
                ("pcache", info.capabilities.prompt_caching),
                ("rsn", info.capabilities.reasoning_effort),
                ("vis", info.capabilities.vision),
                ("tools", info.capabilities.tool_calling),
                ("json", info.capabilities.json_mode),
            ] {
                if present {
                    row.push(Span::styled(
                        format!(" [{tag}]"),
                        Style::default().fg(crate::render::theme::green()),
                    ));
                }
            }
            lines.push(Line::from(row));
        }
        if end < total {
            lines.push(Line::from(Span::styled(
                format!("  ▼ {} more below", total - end),
                Style::default().fg(crate::render::theme::quiet()),
            )));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Type filter · ↑/↓ move · Enter commit (or custom id if no match) · Tab all-providers · Esc cancel",
        Style::default().fg(crate::render::theme::quiet()),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_sidebar(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    let height = area.height as usize;
    let total = CONFIG_SECTIONS.len();
    if height == 0 {
        return;
    }
    let (start, end) = sidebar_window(total, state.section_index, height);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(height);
    if start > 0 {
        lines.push(Line::from(Span::styled(
            "  ▲ more",
            Style::default()
                .fg(crate::render::theme::quiet())
                .add_modifier(Modifier::DIM),
        )));
    }
    for (idx, section) in CONFIG_SECTIONS[start..end].iter().enumerate() {
        let idx = start + idx;
        let active = idx == state.section_index;
        let prefix = if active { "› " } else { "  " };
        let style = if active {
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(muted_fg())
        };
        lines.push(Line::from(vec![
            Span::styled(
                prefix,
                Style::default().fg(if active {
                    crate::render::theme::secondary()
                } else {
                    crate::render::theme::quiet()
                }),
            ),
            Span::styled(section.label, style),
        ]));
    }
    if end < total {
        lines.push(Line::from(Span::styled(
            "  ▼ more",
            Style::default()
                .fg(crate::render::theme::quiet())
                .add_modifier(Modifier::DIM),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn sidebar_window(total: usize, cursor: usize, available_rows: usize) -> (usize, usize) {
    if total == 0 {
        return (0, 0);
    }
    if total <= available_rows {
        return (0, total);
    }
    let cursor = cursor.min(total - 1);
    if available_rows <= 1 {
        return (cursor, cursor + 1);
    }

    let mut row_slots = available_rows.saturating_sub(1).max(1);
    let mut start = centered_scroll_start(total, cursor, row_slots);
    loop {
        let marker_rows = usize::from(start > 0) + usize::from(start + row_slots < total);
        let next_slots = available_rows.saturating_sub(marker_rows).max(1);
        let next_start = centered_scroll_start(total, cursor, next_slots);
        if next_slots == row_slots && next_start == start {
            break;
        }
        row_slots = next_slots;
        start = next_start;
    }
    (start, (start + row_slots).min(total))
}

fn centered_scroll_start(total: usize, cursor: usize, visible_rows: usize) -> usize {
    if total <= visible_rows {
        0
    } else {
        let half = visible_rows / 2;
        cursor.saturating_sub(half).min(total - visible_rows)
    }
}

fn render_field_pane(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    let section = state.current_section();
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            section.label,
            Style::default()
                .fg(crate::render::theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            section.description,
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ]));
    lines.push(Line::raw(""));

    let api_key_label = "api_key";
    let max_label = section
        .fields
        .iter()
        .map(|f| f.label.len())
        .chain(if section.id == SectionId::Models {
            Some(api_key_label.len())
        } else {
            None
        })
        .max()
        .unwrap_or(0);

    let total_rows = state.row_count();
    // When an editor is open, focus the pane on just the active row + the
    // editor block, so the editor is always visible in small viewports.
    let editing = state.editor.is_some() || state.secret_entry.is_some();
    let (rows, hidden_above, hidden_below) = if editing {
        (vec![state.field_index], 0, 0)
    } else {
        let detail_rows = if state.on_synthetic_api_key_row() {
            2usize
        } else {
            3usize
        };
        let row_area = (area.height as usize).saturating_sub(2 + detail_rows);
        let (start, end) = field_row_window(total_rows, state.field_index, row_area);
        (
            (start..end).collect(),
            start,
            total_rows.saturating_sub(end),
        )
    };

    if hidden_above > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ▲ {hidden_above} more above"),
            Style::default().fg(crate::render::theme::quiet()),
        )));
    }
    for row in rows {
        let active = row == state.field_index;
        let prefix = if active { "› " } else { "  " };
        let prefix_style = Style::default().fg(if active {
            crate::render::theme::secondary()
        } else {
            crate::render::theme::quiet()
        });
        match state.field_at_row(row) {
            Some(field) => {
                let (value, source) = state.displayed_value_and_source(field);
                let mut value_str = value.as_display();
                // An empty reroute filter means "reroute from any parent
                // model" — show that explicitly rather than a bare "—".
                if field.toml_path == ["providers", "*", "expensive_models"]
                    && matches!(&value, FieldValue::String(s) if s.is_empty())
                {
                    value_str = "any".to_string();
                }
                // Read-only info rows (e.g. the Routing provider banner) are
                // pinned context, not a setting — render the value in the amber
                // accent + italic and drop the inheritance badge.
                let is_info = matches!(field.kind, squeezy_core::config_schema::FieldKind::Info);
                let source_label = if is_info {
                    String::new()
                } else {
                    inheritance_label(state.scope, source)
                };
                let label_style = if active {
                    Style::default()
                        .fg(crate::render::theme::secondary())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(muted_fg())
                };
                let value_style = if is_info {
                    Style::default()
                        .fg(crate::render::theme::accent())
                        .add_modifier(Modifier::ITALIC)
                } else {
                    Style::default().fg(if active {
                        crate::render::theme::secondary()
                    } else {
                        muted_fg()
                    })
                };
                let mut spans = vec![
                    Span::styled(prefix, prefix_style),
                    Span::styled(
                        format!("{:<width$}", field.label, width = max_label + 2),
                        label_style,
                    ),
                    Span::styled(value_str, value_style),
                ];
                if !source_label.is_empty() {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(source_label, source_style(source)));
                }
                lines.push(Line::from(spans));
            }
            None => {
                // Synthetic API-key row.
                let (provider_label, env_var) =
                    match provider_api_key_env(&state.effective.provider) {
                        Some(t) => (t.0.to_string(), t.1),
                        None => ("—".to_string(), String::new()),
                    };
                // Reflect the *full* runtime resolution chain the provider
                // client uses (`resolve_api_key_with_inline`: inline TOML,
                // credentials.json, the canonical env var, the conventional
                // fallback env var, then SQUEEZY_CREDENTIALS_JSON) so the row
                // agrees with what a real session resolves. Checking only the
                // canonical env var reported "unset" while a working
                // `ANTHROPIC_API_KEY`, credentials.json, or inline key was in
                // effect. Mirrors `doctor`'s credential check. Resolution is
                // memoized on the state (recomputed only when the provider env
                // var or inline key changes), so a config screen left open
                // while a turn animates — which repaints per frame — does not
                // re-read credentials.json each frame. Only the source is
                // used; the secret value is never displayed.
                let inline = super::provider_inline_api_key(&state.effective.provider);
                let source = state.credential_source(&env_var, inline.as_deref());
                let label_style = if active {
                    Style::default()
                        .fg(crate::render::theme::magenta())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(crate::render::theme::magenta())
                };
                let env_text = if env_var.is_empty() {
                    "n/a for this provider".to_string()
                } else if let Some(source) = source {
                    format!("•••• ({})", credential_source_detail(source, &env_var))
                } else {
                    format!("unset ({env_var})")
                };
                let badge = if env_var.is_empty() {
                    String::new()
                } else if let Some(source) = source {
                    format!(
                        "[{} · {}]",
                        credential_source_tag(source),
                        provider_label.to_lowercase()
                    )
                } else {
                    format!("[unset · {}]", provider_label.to_lowercase())
                };
                let mut spans = vec![
                    Span::styled(prefix, prefix_style),
                    Span::styled(
                        format!("{:<width$}", api_key_label, width = max_label + 2),
                        label_style,
                    ),
                    Span::styled(env_text, Style::default().fg(crate::render::theme::quiet())),
                ];
                if !badge.is_empty() {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        badge,
                        Style::default().fg(crate::render::theme::magenta()),
                    ));
                }
                lines.push(Line::from(spans));
            }
        }
    }
    if hidden_below > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ▼ {hidden_below} more below"),
            Style::default().fg(crate::render::theme::quiet()),
        )));
    }

    lines.push(Line::raw(""));
    if state.on_synthetic_api_key_row() {
        let (provider_label, env_var) = match provider_api_key_env(&state.effective.provider) {
            Some(t) => (t.0.to_string(), t.1),
            None => ("this provider".to_string(), "—".to_string()),
        };
        lines.push(Line::from(vec![
            Span::styled("? ", Style::default().fg(crate::render::theme::quiet())),
            Span::styled(
                format!(
                    "Enter / Space sets the API key for {} (env var {}). Saved as \
                     inline `[providers.*].api_key` in the active scope's TOML \
                     (User or Local; Repo is refused).",
                    provider_label, env_var
                ),
                Style::default().fg(crate::render::theme::quiet()),
            ),
        ]));
    } else {
        let field = state.current_field();
        lines.push(Line::from(vec![
            Span::styled("? ", Style::default().fg(crate::render::theme::quiet())),
            Span::styled(
                field.help,
                Style::default().fg(crate::render::theme::quiet()),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled(
                "  apply: ",
                Style::default().fg(crate::render::theme::quiet()),
            ),
            Span::styled(
                field.tier.label(),
                Style::default().fg(tier_color(field.tier)),
            ),
        ]));
    }

    if let Some(editor) = &state.editor {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![Span::styled(
            "── editing ──",
            Style::default().fg(crate::render::theme::accent()),
        )]));
        lines.extend(render_editor_lines(editor));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn field_row_window(total: usize, cursor: usize, available_rows: usize) -> (usize, usize) {
    if total == 0 {
        return (0, 0);
    }
    if total <= available_rows {
        return (0, total);
    }

    let cursor = cursor.min(total - 1);
    if available_rows <= 1 {
        return (cursor, cursor + 1);
    }
    if available_rows == 2 {
        let start = if cursor == 0 { 0 } else { cursor };
        return (start, start + 1);
    }

    let mut row_slots = available_rows.saturating_sub(1).max(1);
    let mut start = scroll_start_for_cursor(total, cursor, row_slots);
    loop {
        let marker_rows = usize::from(start > 0) + usize::from(start + row_slots < total);
        let next_slots = available_rows.saturating_sub(marker_rows).max(1);
        let next_start = scroll_start_for_cursor(total, cursor, next_slots);
        if next_slots == row_slots && next_start == start {
            break;
        }
        row_slots = next_slots;
        start = next_start;
    }
    (start, (start + row_slots).min(total))
}

fn scroll_start_for_cursor(total: usize, cursor: usize, visible_rows: usize) -> usize {
    if total <= visible_rows {
        0
    } else if cursor + 1 > visible_rows {
        (cursor + 1 - visible_rows).min(total - visible_rows)
    } else {
        0
    }
}

/// Build the spans for a single-line text draft with a visible caret at the
/// char-index `cursor`. The caret reverses the glyph it sits on; when the
/// cursor is at the end of the draft it reverses a trailing space so the
/// insertion point is still drawn. `cursor` is clamped to the draft length to
/// stay panic-safe against any stale index.
fn caret_line(draft: &str, cursor: usize) -> Line<'static> {
    let chars: Vec<char> = draft.chars().collect();
    let cursor = cursor.min(chars.len());
    let before: String = chars[..cursor].iter().collect();
    let at: String = chars
        .get(cursor)
        .map(|c| c.to_string())
        .unwrap_or_else(|| " ".to_string());
    let after: String = chars[cursor..].iter().skip(1).collect();
    let caret = Style::default().add_modifier(Modifier::REVERSED);
    Line::from(vec![
        Span::raw("  "),
        Span::raw(before),
        Span::styled(at, caret),
        Span::raw(after),
    ])
}

fn render_editor_lines(editor: &FieldEditor) -> Vec<Line<'static>> {
    match editor {
        FieldEditor::Text { draft, cursor } | FieldEditor::Duration { draft, cursor } => {
            vec![
                caret_line(draft, *cursor),
                Line::from(Span::styled(
                    "Enter to commit · Esc to cancel",
                    Style::default().fg(crate::render::theme::quiet()),
                )),
            ]
        }
        FieldEditor::Integer {
            draft,
            cursor,
            min,
            max,
        }
        | FieldEditor::OptionalInteger {
            draft,
            cursor,
            min,
            max,
        } => {
            vec![
                caret_line(draft, *cursor),
                Line::from(Span::styled(
                    format!("range: {min}..={max} · Enter to commit · Esc to cancel"),
                    Style::default().fg(crate::render::theme::quiet()),
                )),
            ]
        }
        FieldEditor::OptionalFloat {
            draft,
            cursor,
            min,
            max,
        } => {
            vec![
                caret_line(draft, *cursor),
                Line::from(Span::styled(
                    format!("range: {min}..={max} · Enter to commit · Esc to cancel"),
                    Style::default().fg(crate::render::theme::quiet()),
                )),
            ]
        }
        FieldEditor::Enum { options, cursor } => {
            let mut spans = vec![Span::raw("  ")];
            for (i, opt) in options.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw(" "));
                }
                if i == *cursor {
                    spans.push(Span::styled(
                        format!("[{opt}]"),
                        Style::default()
                            .fg(crate::render::theme::secondary())
                            .add_modifier(Modifier::BOLD),
                    ));
                } else {
                    spans.push(Span::styled(
                        format!(" {opt} "),
                        Style::default().fg(muted_fg()),
                    ));
                }
            }
            vec![
                Line::from(spans),
                Line::from(Span::styled(
                    "← / → to move · Enter to commit · Esc to cancel",
                    Style::default().fg(crate::render::theme::quiet()),
                )),
            ]
        }
        FieldEditor::OptionalEnum { options, cursor } => {
            let mut spans = vec![Span::raw("  ")];
            let highlight = |label: String, sel: bool| {
                Span::styled(
                    label,
                    if sel {
                        Style::default()
                            .fg(crate::render::theme::secondary())
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(muted_fg())
                    },
                )
            };
            spans.push(highlight(
                if *cursor == 0 {
                    "[—]".to_string()
                } else {
                    " — ".to_string()
                },
                *cursor == 0,
            ));
            for (i, opt) in options.iter().enumerate() {
                spans.push(Span::raw(" "));
                let sel = *cursor == i + 1;
                spans.push(highlight(
                    if sel {
                        format!("[{opt}]")
                    } else {
                        format!(" {opt} ")
                    },
                    sel,
                ));
            }
            vec![
                Line::from(spans),
                Line::from(Span::styled(
                    "← / → to move · Enter to commit · Esc to cancel",
                    Style::default().fg(crate::render::theme::quiet()),
                )),
            ]
        }
        FieldEditor::Bool(v) => {
            let mut spans = vec![Span::raw("  ")];
            let on_style = if *v {
                Style::default()
                    .fg(crate::render::theme::secondary())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(muted_fg())
            };
            let off_style = if !*v {
                Style::default()
                    .fg(crate::render::theme::secondary())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(muted_fg())
            };
            spans.push(Span::styled(
                if !*v {
                    "[false]".to_string()
                } else {
                    " false ".to_string()
                },
                off_style,
            ));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                if *v {
                    "[true]".to_string()
                } else {
                    " true ".to_string()
                },
                on_style,
            ));
            vec![
                Line::from(spans),
                Line::from(Span::styled(
                    "Space / ← / → to toggle · Enter to commit · Esc to cancel",
                    Style::default().fg(crate::render::theme::quiet()),
                )),
            ]
        }
        FieldEditor::StringList { draft, cursor } => {
            vec![
                caret_line(draft, *cursor),
                Line::from(Span::styled(
                    "comma-separated · Enter to commit · Esc to cancel",
                    Style::default().fg(crate::render::theme::quiet()),
                )),
            ]
        }
        FieldEditor::Path { draft, cursor } => {
            vec![
                caret_line(draft, *cursor),
                Line::from(Span::styled(
                    "filesystem path · Enter to commit · Esc to cancel",
                    Style::default().fg(crate::render::theme::quiet()),
                )),
            ]
        }
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    // While the live filter is open, Tab/Enter/Esc mean iterate/jump/cancel —
    // not the browse-mode scope/edit/close — so swap the hint row to match.
    if state.search.is_some() {
        render_filter_footer(frame, area);
        return;
    }
    // Two rows of bindings — the original single line dropped Ctrl+R,
    // Ctrl+D, BackTab, and labelled the discard binding as a bare "X"
    // even though it requires Shift. Splitting across two lines keeps
    // the most-used navigation visible on narrow terminals while the
    // less-discovered chords stay one glance away.
    let primary = Line::from(vec![
        Span::styled(
            " Tab/Shift+Tab",
            Style::default().fg(crate::render::theme::secondary()),
        ),
        Span::raw(" scope · "),
        Span::styled(
            "↑/↓",
            Style::default().fg(crate::render::theme::secondary()),
        ),
        Span::raw(" field · "),
        Span::styled(
            "←/→",
            Style::default().fg(crate::render::theme::secondary()),
        ),
        Span::raw(" section · "),
        Span::styled(
            "Enter",
            Style::default().fg(crate::render::theme::secondary()),
        ),
        Span::raw(" edit · "),
        Span::styled(
            "Space",
            Style::default().fg(crate::render::theme::secondary()),
        ),
        Span::raw(" cycle · "),
        Span::styled(
            "Type",
            Style::default().fg(crate::render::theme::secondary()),
        ),
        Span::raw(" to filter · "),
        Span::styled(
            "Esc",
            Style::default().fg(crate::render::theme::secondary()),
        ),
        Span::raw(" close "),
    ]);
    let secondary = Line::from(vec![
        Span::styled(
            " Ctrl+R",
            Style::default().fg(crate::render::theme::secondary()),
        ),
        Span::raw(" reset to default · "),
        Span::styled(
            "Ctrl+D",
            Style::default().fg(crate::render::theme::secondary()),
        ),
        Span::raw(" clear override · "),
        Span::styled(
            "Ctrl+Z",
            Style::default().fg(crate::render::theme::secondary()),
        ),
        Span::raw(" undo · "),
        Span::styled(
            "Shift+X",
            Style::default().fg(crate::render::theme::secondary()),
        ),
        Span::raw(" discard all (with y/n) "),
    ]);
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(crate::render::theme::quiet()));
    frame.render_widget(
        Paragraph::new(vec![primary, secondary])
            .style(Style::default().fg(footer_fg()))
            .block(block),
        area,
    );
}

/// Footer shown while the live filter is open: iterate/jump/cancel controls and
/// a reminder of what the query matches.
fn render_filter_footer(frame: &mut Frame<'_>, area: Rect) {
    let key =
        |s: &'static str| Span::styled(s, Style::default().fg(crate::render::theme::secondary()));
    let primary = Line::from(vec![
        key(" ↑/↓ or Tab"),
        Span::raw(" move · "),
        key("Enter"),
        Span::raw(" jump to setting · "),
        key("Esc"),
        Span::raw(" cancel "),
    ]);
    let secondary = Line::from(vec![
        key(" Backspace"),
        Span::raw(" delete · keep typing to refine · matches "),
        key("section"),
        Span::raw(" / "),
        key("option"),
        Span::raw(" / "),
        key("value"),
        Span::raw(" / "),
        key("description"),
        Span::raw(" "),
    ]);
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(crate::render::theme::quiet()));
    frame.render_widget(
        Paragraph::new(vec![primary, secondary])
            .style(Style::default().fg(footer_fg()))
            .block(block),
        area,
    );
}

/// Detail shown after the masked key on the synthetic API-key row, naming
/// the source the runtime resolved the credential from. Mirrors `doctor`'s
/// `key_source_label` so `/config` and `doctor` agree. For a fallback env
/// var it surfaces the conventional vendor name (e.g. `ANTHROPIC_API_KEY`)
/// the user actually exported, not the canonical `SQUEEZY_*` name.
fn credential_source_detail(source: squeezy_llm::KeySource, env_var: &str) -> String {
    use squeezy_llm::KeySource;
    match source {
        KeySource::Inline => env_var.to_string(),
        KeySource::Env => format!("{env_var} — from environment"),
        KeySource::FallbackEnv => squeezy_llm::fallback_env_var(env_var)
            .map(|name| format!("{name} — from environment"))
            .unwrap_or_else(|| format!("{env_var} — from environment")),
        KeySource::File => "credentials.json".to_string(),
        KeySource::JsonEnv => "SQUEEZY_CREDENTIALS_JSON".to_string(),
    }
}

/// One-word badge tag for a resolved credential source.
fn credential_source_tag(source: squeezy_llm::KeySource) -> &'static str {
    use squeezy_llm::KeySource;
    match source {
        KeySource::Inline => "toml",
        KeySource::Env | KeySource::FallbackEnv => "env",
        KeySource::File => "file",
        KeySource::JsonEnv => "json",
    }
}

fn source_style(source: FieldSource) -> Style {
    match source {
        FieldSource::Default => Style::default().fg(crate::render::theme::quiet()),
        FieldSource::User => Style::default().fg(crate::render::theme::accent()),
        FieldSource::Project => Style::default().fg(crate::render::theme::secondary()),
        FieldSource::Repo => Style::default().fg(crate::render::theme::green()),
        // Env overrides are informational ("this value comes from $SQUEEZY_*"),
        // not an error. Painting them crate::render::theme::red() used to look like a warning
        // banner on otherwise-fine rows. crate::render::theme::magenta() matches how the API-key
        // synthetic row already flags an env-derived secret.
        FieldSource::Env => Style::default().fg(crate::render::theme::magenta()),
    }
}

fn tier_color(tier: ApplyTier) -> Color {
    match tier {
        ApplyTier::Immediate => crate::render::theme::green(),
        ApplyTier::NextPrompt => crate::render::theme::accent(),
        ApplyTier::Restart => crate::render::theme::secondary(),
    }
}

/// Full-screen multi-line editor surface for long String fields (the judge
/// prompt). Soft-wraps the buffer to the pane width, draws a reversed-cell
/// caret, and scrolls vertically to keep the cursor on screen.
fn render_prompt_editor(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    let Some(ed) = state.prompt_editor.as_ref() else {
        return;
    };
    let field = state.current_field();
    let provider = super::active_provider_slug(&state.effective);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(crate::render::theme::quiet()))
        .title(Span::styled(
            format!("  Edit {} · {provider}  ", field.label),
            Style::default()
                .fg(crate::render::theme::accent())
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    let text_area = chunks[0];
    let footer_area = chunks[1];

    let width = text_area.width.max(1) as usize;
    let height = (text_area.height.max(1)) as usize;

    let rows = wrap_rows(&ed.draft, width);
    let cursor_row = cursor_display_row(&rows, ed.cursor);
    // Anchor the viewport so the cursor row is always visible.
    let scroll = if cursor_row < height {
        0
    } else {
        cursor_row - height + 1
    };

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(height);
    for (row_idx, &(s, e)) in rows.iter().enumerate().skip(scroll).take(height) {
        let segment = &ed.draft[s..e];
        if row_idx == cursor_row {
            let col = ed.cursor.saturating_sub(s).min(segment.len());
            let before = segment[..col].to_string();
            let rest = &segment[col..];
            let at = rest
                .chars()
                .next()
                .map(|c| c.to_string())
                .unwrap_or_else(|| " ".to_string());
            let after: String = rest.chars().skip(1).collect();
            lines.push(Line::from(vec![
                Span::raw(before),
                Span::styled(at, Style::default().add_modifier(Modifier::REVERSED)),
                Span::raw(after),
            ]));
        } else {
            lines.push(Line::from(Span::raw(segment.to_string())));
        }
    }
    frame.render_widget(Paragraph::new(lines), text_area);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Enter newline · Ctrl+S save · Esc discards edits · ↑↓←→ move · clear+save resets to built-in",
            Style::default().fg(crate::render::theme::quiet()),
        ))),
        footer_area,
    );
}

/// Soft-wrap `text` to `width` columns, returning the byte range `[start, end)`
/// of each display row. A newline starts a new row; a logical line longer than
/// `width` splits across rows; an empty logical line yields an empty row so the
/// cursor can rest on it. Width is counted in chars (≈ columns).
fn wrap_rows(text: &str, width: usize) -> Vec<(usize, usize)> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut line_start = 0;
    loop {
        let nl = text[line_start..].find('\n').map(|o| line_start + o);
        let line_end = nl.unwrap_or(text.len());
        let mut seg_start = line_start;
        loop {
            let mut seg_end = seg_start;
            for (count, (i, ch)) in text[seg_start..line_end].char_indices().enumerate() {
                if count == width {
                    break;
                }
                seg_end = seg_start + i + ch.len_utf8();
            }
            rows.push((seg_start, seg_end));
            if seg_end >= line_end {
                break;
            }
            seg_start = seg_end;
        }
        match nl {
            Some(pos) => {
                line_start = pos + 1;
                // A trailing newline leaves an empty final row for the cursor.
                if line_start == text.len() {
                    rows.push((line_start, line_start));
                    break;
                }
            }
            None => break,
        }
    }
    if rows.is_empty() {
        rows.push((0, 0));
    }
    rows
}

/// Which display row the cursor sits on. At a soft-wrap boundary (cursor lands
/// exactly where a row ends and the next begins with no newline between) prefer
/// the next row, so the caret shows at column 0 of the continuation.
fn cursor_display_row(rows: &[(usize, usize)], cursor: usize) -> usize {
    for (i, &(s, e)) in rows.iter().enumerate() {
        if cursor >= s && cursor <= e {
            if cursor == e
                && let Some(&(ns, _)) = rows.get(i + 1)
                && ns == cursor
            {
                continue;
            }
            return i;
        }
    }
    rows.len().saturating_sub(1)
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
