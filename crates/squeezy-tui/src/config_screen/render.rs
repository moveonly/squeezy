use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use squeezy_core::config_schema::{ApplyTier, CONFIG_SECTIONS, FieldSource, SectionId};

use super::RESET_ACTIONS;
use super::{
    ConfigScope, ConfigScreenState, FieldEditor, ModelPickerState, SearchOverlayState,
    SecretEntryState, inheritance_label, picker_matches, provider_api_key_env, tier_path,
};
use crate::render::palette::{
    AMBER, GOLD, MODE_PURPLE, QUIET, SEPARATOR_BLUE, SUCCESS_GREEN, footer_fg, muted_fg,
};

/// Pretty-print an absolute config path: replace `$HOME` with `~` so the
/// tab subtitle stays compact, while still surfacing the per-machine
/// project hash for the Local tier so the user can grep `~/.squeezy/projects/`
/// for the exact directory.
fn display_path(path: &std::path::Path) -> String {
    let full = path.display().to_string();
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
        && let Some(rest) = full.strip_prefix(&home)
    {
        return format!("~{rest}");
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
    fn tab(
        label: &'static str,
        subtitle: String,
        active: bool,
        exists: bool,
    ) -> Vec<Span<'static>> {
        // The active tab is identified by the amber dot alone — we used to
        // also stamp an extra "▸ " in front of the label, but the ▸
        // separators between tabs already make that look like "▸ ▸ Repo"
        // when the middle/last tab is active. Dropping the active marker
        // keeps the row aligned and leaves a single clear indicator.
        let label_style = if active {
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(muted_fg())
        };
        let dot = if exists { "●" } else { "○" };
        // Active dot is amber, inactive dots are quiet (grey). File
        // existence is still encoded via ●/○ shape, but the colour
        // dimension is reserved for "this is the tab you're editing".
        let dot_style = if active {
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(QUIET)
        };
        let subtitle_text = if subtitle.is_empty() {
            String::new()
        } else {
            format!(" {subtitle}")
        };
        vec![
            Span::styled(label, label_style),
            Span::raw(" "),
            Span::styled(dot, dot_style),
            Span::styled(subtitle_text, Style::default().fg(QUIET)),
        ]
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

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        "  Config  ",
        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(" │ ", Style::default().fg(QUIET)));
    spans.extend(tab(
        "User",
        user_sub,
        state.scope == ConfigScope::User,
        user_exists,
    ));
    spans.push(Span::styled(" ▸ ", Style::default().fg(SEPARATOR_BLUE)));
    let repo_subtitle = if repo_sub.is_empty() {
        String::new()
    } else if repo_exists {
        format!("{repo_sub} (committed)")
    } else {
        repo_sub.clone()
    };
    spans.extend(tab(
        "Repo",
        repo_subtitle,
        state.scope == ConfigScope::Repo,
        repo_exists,
    ));
    spans.push(Span::styled(" ▸ ", Style::default().fg(SEPARATOR_BLUE)));
    spans.extend(tab(
        "Local",
        local_sub,
        state.scope == ConfigScope::Local,
        local_exists,
    ));
    if state.dirty {
        spans.push(Span::styled(
            "    (changes applied)",
            Style::default().fg(QUIET),
        ));
    }
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(QUIET));
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
        Paragraph::new(sep_lines).style(Style::default().fg(QUIET)),
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
    } else if let Some(search) = &state.search {
        render_search_overlay(frame, chunks[2], search);
    } else if state.current_section().id == SectionId::Reset {
        render_reset_section(frame, chunks[2], state);
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
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(section.description, Style::default().fg(QUIET)),
    ]));
    lines.push(Line::raw(""));

    let tier_path = tier_path(state, action.scope);
    let exists = std::fs::metadata(&tier_path).is_ok();
    let status = if exists {
        Span::styled("[file present]", Style::default().fg(SUCCESS_GREEN))
    } else {
        Span::styled("[no file]", Style::default().fg(QUIET))
    };
    lines.push(Line::from(vec![
        Span::styled("› ", Style::default().fg(GOLD)),
        Span::styled(
            format!("{:<28}", action.label),
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
        ),
        status,
    ]));
    lines.push(Line::from(vec![
        Span::raw("    "),
        Span::styled(action.detail, Style::default().fg(QUIET)),
    ]));
    lines.push(Line::from(vec![
        Span::raw("    "),
        Span::styled(tier_path.display().to_string(), Style::default().fg(QUIET)),
    ]));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("? ", Style::default().fg(QUIET)),
        Span::styled(
            "Enter to delete this tier's file (with y/n confirmation). Ctrl+Z restores it.",
            Style::default().fg(QUIET),
        ),
    ]));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
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
        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw("  Delete the "),
        Span::styled(
            scope.label(),
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" settings file?"),
    ]));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("    path   ", Style::default().fg(QUIET)),
        Span::raw(path.display().to_string()),
    ]));
    lines.push(Line::from(vec![
        Span::styled("    status ", Style::default().fg(QUIET)),
        Span::styled(
            if exists { "exists" } else { "(no file)" },
            Style::default().fg(if exists { SUCCESS_GREEN } else { QUIET }),
        ),
    ]));
    lines.push(Line::raw(""));

    if !exists {
        lines.push(Line::from(Span::styled(
            "  Nothing to delete — that tier file does not exist on disk. \
             Confirming is harmless: the effective config doesn't change.",
            Style::default().fg(QUIET),
        )));
    } else if preview.is_empty() {
        lines.push(Line::from(Span::styled(
            "  The file exists, but every key in it matches the value that \
             would still be effective after deletion (env override, identical \
             higher-priority tier value, or the binary default). \
             Confirming deletes the file without changing any displayed value.",
            Style::default().fg(QUIET),
        )));
    } else {
        let plural = if preview.len() == 1 { "" } else { "s" };
        lines.push(Line::from(vec![Span::styled(
            format!(
                "  {} key{plural} will change effective value:",
                preview.len()
            ),
            Style::default().fg(AMBER),
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
                Span::styled(entry.before.clone(), Style::default().fg(GOLD)),
                Span::raw("  →  "),
                Span::styled(entry.after.clone(), Style::default().fg(SUCCESS_GREEN)),
                Span::raw(" "),
                Span::styled(after_label, source_style(entry.after_source)),
            ]));
        }
        if preview.len() > max_rows {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                format!("    … and {} more", preview.len() - max_rows),
                Style::default().fg(QUIET),
            )));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![Span::styled(
        "  Other tabs are not touched. Ctrl+Z restores the deleted file.",
        Style::default().fg(QUIET),
    )]));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("y", Style::default().fg(GOLD).add_modifier(Modifier::BOLD)),
        Span::styled(" delete   ", Style::default().fg(QUIET)),
        Span::styled("n", Style::default().fg(GOLD).add_modifier(Modifier::BOLD)),
        Span::styled(" cancel   ", Style::default().fg(QUIET)),
        Span::styled(
            "Esc",
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" cancel", Style::default().fg(QUIET)),
    ]));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_discard_confirm(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        "Discard all session writes",
        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::raw(""));
    let count = state.undo_stack.len();
    let plural = if count == 1 { "" } else { "s" };
    lines.push(Line::from(vec![
        Span::raw("  Revert "),
        Span::styled(
            format!("{count} write{plural}"),
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
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
            Span::styled("    file  ", Style::default().fg(QUIET)),
            Span::raw(path.display().to_string()),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  Each tier file is restored to the bytes captured when /config opened.",
        Style::default().fg(QUIET),
    )));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("y", Style::default().fg(GOLD).add_modifier(Modifier::BOLD)),
        Span::styled(" discard   ", Style::default().fg(QUIET)),
        Span::styled("n", Style::default().fg(GOLD).add_modifier(Modifier::BOLD)),
        Span::styled(" cancel   ", Style::default().fg(QUIET)),
        Span::styled(
            "Esc",
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" cancel", Style::default().fg(QUIET)),
    ]));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_secret_entry(frame: &mut Frame<'_>, area: Rect, entry: &SecretEntryState) {
    let display: String = if entry.reveal {
        // Explicit Ctrl+T toggle — show the full plaintext for verification.
        entry.draft.clone()
    } else {
        "•".repeat(entry.char_len())
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(
                "Set API key",
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!("for {}", entry.provider_label),
                Style::default().fg(QUIET),
            ),
        ]),
        Line::from(vec![
            Span::styled("env  ", Style::default().fg(QUIET)),
            Span::styled(entry.env_var.as_str(), Style::default().fg(muted_fg())),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(display, Style::default().fg(muted_fg())),
            Span::styled("_", Style::default().fg(AMBER)),
        ]),
        Line::raw(""),
        Line::from(vec![Span::styled(
            "Paste your key. Saved as inline `api_key` in the active scope's settings \
             TOML (User or Local). Refuses the committed repo TOML. The running \
             provider client is rebuilt on the next prompt.",
            Style::default().fg(QUIET),
        )]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("Enter ", Style::default().fg(GOLD)),
            Span::styled("save  ", Style::default().fg(QUIET)),
            Span::styled("Ctrl+T ", Style::default().fg(GOLD)),
            Span::styled(
                if entry.reveal {
                    "hide key  "
                } else {
                    "reveal full key  "
                },
                Style::default().fg(QUIET),
            ),
            Span::styled("Esc ", Style::default().fg(GOLD)),
            Span::styled("cancel", Style::default().fg(QUIET)),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_search_overlay(frame: &mut Frame<'_>, area: Rect, search: &SearchOverlayState) {
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(search.matches.len() + 3);
    lines.push(Line::from(vec![
        Span::styled(
            "Search",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("fuzzy match field labels", Style::default().fg(QUIET)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("/", Style::default().fg(QUIET)),
        Span::raw(search.query.clone()),
        Span::styled("_", Style::default().fg(AMBER)),
    ]));
    lines.push(Line::raw(""));
    if search.matches.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matches",
            Style::default().fg(QUIET),
        )));
    } else {
        // Reserve rows for: 3 above (header + query + blank) and 2 below
        // (blank + help). Below that ceiling, scroll the match window
        // around the active cursor so navigating past the visible region
        // doesn't park the highlight off-screen.
        let chrome = 5u16;
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
                format!("  ▲ {} more above", start),
                Style::default().fg(QUIET),
            )));
        }
        for (idx, (sidx, fidx, _score)) in search.matches[start..end].iter().enumerate() {
            let row_idx = start + idx;
            let section = &CONFIG_SECTIONS[*sidx];
            let field = &section.fields[*fidx];
            let active = row_idx == cursor;
            let prefix = if active { "› " } else { "  " };
            let style = if active {
                Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(muted_fg())
            };
            lines.push(Line::from(vec![
                Span::styled(
                    prefix,
                    Style::default().fg(if active { GOLD } else { QUIET }),
                ),
                Span::styled(format!("{:<22}", section.label), Style::default().fg(QUIET)),
                Span::styled(format!("{:<28}", field.label), style),
            ]));
        }
        if end < total {
            lines.push(Line::from(Span::styled(
                format!("  ▼ {} more below", total - end),
                Style::default().fg(QUIET),
            )));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Type to filter · ↑/↓ move · Enter jump · Esc cancel",
        Style::default().fg(QUIET),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
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
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(format!("scope: {scope_label}"), Style::default().fg(QUIET)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("filter ", Style::default().fg(QUIET)),
        Span::raw("› "),
        Span::raw(picker.filter.clone()),
        Span::styled("_", Style::default().fg(AMBER)),
    ]));
    lines.push(Line::raw(""));
    if matches.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matches · Enter to commit the filter as a custom model id",
            Style::default().fg(QUIET),
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
                Style::default().fg(QUIET),
            )));
        }
        for (idx, info) in matches[start..end].iter().enumerate() {
            let row_idx = start + idx;
            let active = row_idx == cursor;
            let prefix = if active { "› " } else { "  " };
            let style = if active {
                Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(muted_fg())
            };
            let mut row = vec![
                Span::styled(
                    prefix,
                    Style::default().fg(if active { GOLD } else { QUIET }),
                ),
                Span::styled(format!("{:<32}", info.id), style),
            ];
            if picker.all_providers {
                row.push(Span::styled(
                    format!("{:<12}", info.provider),
                    Style::default().fg(QUIET),
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
                        Style::default().fg(SUCCESS_GREEN),
                    ));
                }
            }
            lines.push(Line::from(row));
        }
        if end < total {
            lines.push(Line::from(Span::styled(
                format!("  ▼ {} more below", total - end),
                Style::default().fg(QUIET),
            )));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Type filter · ↑/↓ move · Enter commit (or custom id if no match) · Tab all-providers · Esc cancel",
        Style::default().fg(QUIET),
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
            Style::default().fg(QUIET).add_modifier(Modifier::DIM),
        )));
    }
    for (idx, section) in CONFIG_SECTIONS[start..end].iter().enumerate() {
        let idx = start + idx;
        let active = idx == state.section_index;
        let prefix = if active { "› " } else { "  " };
        let style = if active {
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(muted_fg())
        };
        lines.push(Line::from(vec![
            Span::styled(
                prefix,
                Style::default().fg(if active { GOLD } else { QUIET }),
            ),
            Span::styled(section.label, style),
        ]));
    }
    if end < total {
        lines.push(Line::from(Span::styled(
            "  ▼ more",
            Style::default().fg(QUIET).add_modifier(Modifier::DIM),
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
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(section.description, Style::default().fg(QUIET)),
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
            Style::default().fg(QUIET),
        )));
    }
    for row in rows {
        let active = row == state.field_index;
        let prefix = if active { "› " } else { "  " };
        let prefix_style = Style::default().fg(if active { GOLD } else { QUIET });
        match state.field_at_row(row) {
            Some(field) => {
                let (value, source) = state.displayed_value_and_source(field);
                let value_str = value.as_display();
                let source_label = inheritance_label(state.scope, source);
                let label_style = if active {
                    Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(muted_fg())
                };
                let mut spans = vec![
                    Span::styled(prefix, prefix_style),
                    Span::styled(
                        format!("{:<width$}", field.label, width = max_label + 2),
                        label_style,
                    ),
                    Span::styled(
                        value_str,
                        Style::default().fg(if active { GOLD } else { muted_fg() }),
                    ),
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
                let inline_present =
                    super::provider_inline_api_key(&state.effective.provider).is_some();
                // The label said "unset" even when the user had exported
                // the env var in their shell. A key present in $OPENAI_API_KEY
                // is exactly what the provider client picks up at startup,
                // so report it as such — the inline TOML write is only one
                // way to populate the credential.
                let env_present =
                    !env_var.is_empty() && std::env::var(&env_var).is_ok_and(|v| !v.is_empty());
                let label_style = if active {
                    Style::default()
                        .fg(MODE_PURPLE)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MODE_PURPLE)
                };
                let env_text = if env_var.is_empty() {
                    "n/a for this provider".to_string()
                } else if inline_present {
                    format!("•••• ({env_var})")
                } else if env_present {
                    format!("•••• ({env_var} — from environment)")
                } else {
                    format!("unset ({env_var})")
                };
                let badge = if env_var.is_empty() {
                    String::new()
                } else if inline_present {
                    format!("[toml · {}]", provider_label.to_lowercase())
                } else if env_present {
                    format!("[env · {}]", provider_label.to_lowercase())
                } else {
                    format!("[unset · {}]", provider_label.to_lowercase())
                };
                let mut spans = vec![
                    Span::styled(prefix, prefix_style),
                    Span::styled(
                        format!("{:<width$}", api_key_label, width = max_label + 2),
                        label_style,
                    ),
                    Span::styled(env_text, Style::default().fg(QUIET)),
                ];
                if !badge.is_empty() {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(badge, Style::default().fg(MODE_PURPLE)));
                }
                lines.push(Line::from(spans));
            }
        }
    }
    if hidden_below > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ▼ {hidden_below} more below"),
            Style::default().fg(QUIET),
        )));
    }

    lines.push(Line::raw(""));
    if state.on_synthetic_api_key_row() {
        let (provider_label, env_var) = match provider_api_key_env(&state.effective.provider) {
            Some(t) => (t.0.to_string(), t.1),
            None => ("this provider".to_string(), "—".to_string()),
        };
        lines.push(Line::from(vec![
            Span::styled("? ", Style::default().fg(QUIET)),
            Span::styled(
                format!(
                    "Enter / Space sets the API key for {} (env var {}). Saved as \
                     inline `[providers.*].api_key` in the active scope's TOML \
                     (User or Local; Repo is refused).",
                    provider_label, env_var
                ),
                Style::default().fg(QUIET),
            ),
        ]));
    } else {
        let field = state.current_field();
        lines.push(Line::from(vec![
            Span::styled("? ", Style::default().fg(QUIET)),
            Span::styled(field.help, Style::default().fg(QUIET)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  apply: ", Style::default().fg(QUIET)),
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
            Style::default().fg(AMBER),
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

fn render_editor_lines(editor: &FieldEditor) -> Vec<Line<'static>> {
    match editor {
        FieldEditor::Text { draft, cursor } | FieldEditor::Duration { draft, cursor } => {
            let cursor_str = format!("  {draft}");
            let _ = cursor;
            vec![
                Line::from(Span::raw(cursor_str)),
                Line::from(Span::styled(
                    "Enter to commit · Esc to cancel",
                    Style::default().fg(QUIET),
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
            let _ = cursor;
            vec![
                Line::from(Span::raw(format!("  {draft}"))),
                Line::from(Span::styled(
                    format!("range: {min}..={max} · Enter to commit · Esc to cancel"),
                    Style::default().fg(QUIET),
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
                        Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
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
                    Style::default().fg(QUIET),
                )),
            ]
        }
        FieldEditor::OptionalEnum { options, cursor } => {
            let mut spans = vec![Span::raw("  ")];
            let highlight = |label: String, sel: bool| {
                Span::styled(
                    label,
                    if sel {
                        Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
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
                    Style::default().fg(QUIET),
                )),
            ]
        }
        FieldEditor::Bool(v) => {
            let mut spans = vec![Span::raw("  ")];
            let on_style = if *v {
                Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(muted_fg())
            };
            let off_style = if !*v {
                Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
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
                    Style::default().fg(QUIET),
                )),
            ]
        }
        FieldEditor::StringList { draft, cursor } => {
            let _ = cursor;
            vec![
                Line::from(Span::raw(format!("  {draft}"))),
                Line::from(Span::styled(
                    "comma-separated · Enter to commit · Esc to cancel",
                    Style::default().fg(QUIET),
                )),
            ]
        }
        FieldEditor::Path { draft, cursor } => {
            let _ = cursor;
            vec![
                Line::from(Span::raw(format!("  {draft}"))),
                Line::from(Span::styled(
                    "filesystem path · Enter to commit · Esc to cancel",
                    Style::default().fg(QUIET),
                )),
            ]
        }
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, _state: &ConfigScreenState) {
    // Two rows of bindings — the original single line dropped Ctrl+R,
    // Ctrl+D, BackTab, and labelled the discard binding as a bare "X"
    // even though it requires Shift. Splitting across two lines keeps
    // the most-used navigation visible on narrow terminals while the
    // less-discovered chords stay one glance away.
    let primary = Line::from(vec![
        Span::styled(" Tab/Shift+Tab", Style::default().fg(GOLD)),
        Span::raw(" scope · "),
        Span::styled("↑/↓", Style::default().fg(GOLD)),
        Span::raw(" field · "),
        Span::styled("←/→", Style::default().fg(GOLD)),
        Span::raw(" section · "),
        Span::styled("Enter", Style::default().fg(GOLD)),
        Span::raw(" edit · "),
        Span::styled("Space", Style::default().fg(GOLD)),
        Span::raw(" cycle · "),
        Span::styled("/", Style::default().fg(GOLD)),
        Span::raw(" search · "),
        Span::styled("Esc", Style::default().fg(GOLD)),
        Span::raw(" close "),
    ]);
    let secondary = Line::from(vec![
        Span::styled(" Ctrl+R", Style::default().fg(GOLD)),
        Span::raw(" reset to default · "),
        Span::styled("Ctrl+D", Style::default().fg(GOLD)),
        Span::raw(" clear override · "),
        Span::styled("Ctrl+Z", Style::default().fg(GOLD)),
        Span::raw(" undo · "),
        Span::styled("Shift+X", Style::default().fg(GOLD)),
        Span::raw(" discard all (with y/n) "),
    ]);
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(QUIET));
    frame.render_widget(
        Paragraph::new(vec![primary, secondary])
            .style(Style::default().fg(footer_fg()))
            .block(block),
        area,
    );
}

fn source_style(source: FieldSource) -> Style {
    match source {
        FieldSource::Default => Style::default().fg(QUIET),
        FieldSource::User => Style::default().fg(AMBER),
        FieldSource::Project => Style::default().fg(GOLD),
        FieldSource::Repo => Style::default().fg(SUCCESS_GREEN),
        // Env overrides are informational ("this value comes from $SQUEEZY_*"),
        // not an error. Painting them ERROR_RED used to look like a warning
        // banner on otherwise-fine rows. MODE_PURPLE matches how the API-key
        // synthetic row already flags an env-derived secret.
        FieldSource::Env => Style::default().fg(MODE_PURPLE),
    }
}

fn tier_color(tier: ApplyTier) -> Color {
    match tier {
        ApplyTier::Immediate => SUCCESS_GREEN,
        ApplyTier::NextPrompt => AMBER,
        ApplyTier::Restart => GOLD,
    }
}
