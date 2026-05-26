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
    AMBER, ERROR_RED, GOLD, MODE_PURPLE, QUIET, SEPARATOR_BLUE, SUCCESS_GREEN,
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

pub(crate) fn render(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // tab strip
            Constraint::Min(0),    // body
            Constraint::Length(2), // footer
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
            Style::default().fg(Color::White)
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
        vec![
            Span::styled(label, label_style),
            Span::raw(" "),
            Span::styled(dot, dot_style),
            Span::styled(format!(" {subtitle}"), Style::default().fg(QUIET)),
        ]
    }
    let user_exists = std::fs::metadata(&state.sources.user_path_default).is_ok();
    let repo_exists = std::fs::metadata(&state.sources.project_path_default).is_ok();
    let local_exists = std::fs::metadata(&state.sources.repo_path_default).is_ok();
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        "  Config  ",
        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(" │ ", Style::default().fg(QUIET)));
    spans.extend(tab(
        "User",
        display_path(&state.sources.user_path_default),
        state.scope == ConfigScope::User,
        user_exists,
    ));
    spans.push(Span::styled(" ▸ ", Style::default().fg(SEPARATOR_BLUE)));
    spans.extend(tab(
        "Repo",
        format!(
            "{} (committed)",
            display_path(&state.sources.project_path_default)
        ),
        state.scope == ConfigScope::Repo,
        repo_exists,
    ));
    spans.push(Span::styled(" ▸ ", Style::default().fg(SEPARATOR_BLUE)));
    spans.extend(tab(
        "Local",
        display_path(&state.sources.repo_path_default),
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
                    Style::default().fg(Color::White),
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
            Span::styled("keychain → ", Style::default().fg(QUIET)),
            Span::styled(entry.env_var.as_str(), Style::default().fg(Color::White)),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(display, Style::default().fg(Color::White)),
            Span::styled("_", Style::default().fg(AMBER)),
        ]),
        Line::raw(""),
        Line::from(vec![Span::styled(
            "Paste your key. Stored in the OS keychain only — never written to any TOML \
             or transcript. The running provider client is rebuilt on the next prompt.",
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
        for (idx, (sidx, fidx, _score)) in search.matches.iter().enumerate() {
            let section = &CONFIG_SECTIONS[*sidx];
            let field = &section.fields[*fidx];
            let active = idx == search.cursor.min(search.matches.len() - 1);
            let prefix = if active { "› " } else { "  " };
            let style = if active {
                Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
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
            "  no matches · Ctrl+Enter to commit the filter as a custom model id",
            Style::default().fg(QUIET),
        )));
    } else {
        for (idx, info) in matches.iter().enumerate() {
            let active = idx == picker.cursor.min(matches.len() - 1);
            let prefix = if active { "› " } else { "  " };
            let style = if active {
                Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
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
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Type filter · ↑/↓ move · Enter commit · Tab all-providers · Ctrl+Enter custom · Esc cancel",
        Style::default().fg(QUIET),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_sidebar(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    let mut lines = Vec::with_capacity(CONFIG_SECTIONS.len());
    for (idx, section) in CONFIG_SECTIONS.iter().enumerate() {
        let active = idx == state.section_index;
        let prefix = if active { "› " } else { "  " };
        let style = if active {
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![
            Span::styled(
                prefix,
                Style::default().fg(if active { GOLD } else { QUIET }),
            ),
            Span::styled(section.label, style),
        ]));
    }
    // The sidebar lists CONFIG_SECTIONS verbatim; on shorter terminals it
    // overflows and items at the bottom (notably Reset) get clipped. Pin
    // the active row inside the visible window by scrolling just enough
    // to keep it on-screen.
    let height = area.height as usize;
    let total = lines.len();
    let scroll = if height == 0 || total <= height {
        0u16
    } else {
        state
            .section_index
            .saturating_sub(height - 1)
            .min(total - height) as u16
    };
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
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
    let rows: Vec<usize> = if editing {
        vec![state.field_index]
    } else {
        (0..total_rows).collect()
    };

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
                    Style::default().fg(Color::White)
                };
                let mut spans = vec![
                    Span::styled(prefix, prefix_style),
                    Span::styled(
                        format!("{:<width$}", field.label, width = max_label + 2),
                        label_style,
                    ),
                    Span::styled(
                        value_str,
                        Style::default().fg(if active { GOLD } else { Color::White }),
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
                let label_style = if active {
                    Style::default()
                        .fg(MODE_PURPLE)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MODE_PURPLE)
                };
                let env_text = if env_var.is_empty() {
                    "n/a for this provider".to_string()
                } else {
                    format!("•••• ({env_var})")
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, prefix_style),
                    Span::styled(
                        format!("{:<width$}", api_key_label, width = max_label + 2),
                        label_style,
                    ),
                    Span::styled(env_text, Style::default().fg(QUIET)),
                    Span::raw(" "),
                    Span::styled(
                        format!("[keychain · {}]", provider_label.to_lowercase()),
                        Style::default().fg(MODE_PURPLE),
                    ),
                ]));
            }
        }
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
                    "Enter / Space sets the API key for {} (keychain account {}). \
                     The plaintext never lands in any TOML or transcript.",
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
                        Style::default().fg(Color::White),
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
                        Style::default().fg(Color::White)
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
                Style::default().fg(Color::White)
            };
            let off_style = if !*v {
                Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
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
    let hint = Line::from(vec![
        Span::styled(" Tab", Style::default().fg(GOLD)),
        Span::raw(" scope · "),
        Span::styled("↑/↓", Style::default().fg(GOLD)),
        Span::raw(" field · "),
        Span::styled("Enter", Style::default().fg(GOLD)),
        Span::raw(" edit · "),
        Span::styled("Space", Style::default().fg(GOLD)),
        Span::raw(" cycle · "),
        Span::styled("/", Style::default().fg(GOLD)),
        Span::raw(" search · "),
        Span::styled("Ctrl+Z", Style::default().fg(GOLD)),
        Span::raw(" undo · "),
        Span::styled("X", Style::default().fg(GOLD)),
        Span::raw(" discard · "),
        Span::styled("Esc", Style::default().fg(GOLD)),
        Span::raw(" close "),
    ]);
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(QUIET));
    frame.render_widget(
        Paragraph::new(hint)
            .style(Style::default().fg(Color::White))
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
        FieldSource::Env => Style::default().fg(ERROR_RED),
    }
}

fn tier_color(tier: ApplyTier) -> Color {
    match tier {
        ApplyTier::Immediate => SUCCESS_GREEN,
        ApplyTier::NextPrompt => AMBER,
        ApplyTier::Restart => GOLD,
    }
}
