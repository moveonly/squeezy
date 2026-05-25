//! Full-page configuration UI invoked via `/config` or F11.
//!
//! Layout: two scope tabs (User / Project) on top, a section sidebar on the
//! left, a field editor on the right, and a footer hint row at the bottom.
//! Saves write the corresponding TOML file via `squeezy_core::settings_writer`
//! and apply changes by tier: `Immediate` → `agent.replace_config(...)`;
//! `NextPrompt` → `agent.arm_config_swap(...)`; `Restart` → notification only.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use squeezy_agent::{Agent, PendingConfigSwap};
use squeezy_core::{
    AppConfig, SeparatedSources,
    config_schema::{
        ApplyTier, CONFIG_SECTIONS, ConfigSectionMeta, FieldKind, FieldMeta, FieldSource,
        FieldValue, SectionId,
    },
    load_separated_settings_sources, resolve_field_source,
    settings_writer::{EditOp, SettingsEdit, SettingsScope, WriteOutcome, apply_edits},
};

use crate::{
    notification::{NotificationQueue, Severity as NotifySeverity},
    render::palette::{AMBER, ERROR_RED, GOLD, QUIET, SUCCESS_GREEN},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigScope {
    User,
    Project,
}

impl ConfigScope {
    #[allow(dead_code)] // used by header rendering once the layout adds inline scope chips
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Project => "Project",
        }
    }
}

pub(crate) struct ConfigScreenState {
    pub scope: ConfigScope,
    pub section_index: usize,
    pub field_index: usize,
    pub editor: Option<FieldEditor>,
    pub picker: Option<ModelPickerState>,
    pub search: Option<SearchOverlayState>,
    pub secret_entry: Option<SecretEntryState>,
    pub effective: AppConfig,
    pub sources: SeparatedSources,
    pub dirty: bool,
}

/// Masked text entry for an API key. The plaintext lives only in `draft`
/// and is written to the OS keychain on commit — never to TOML, the
/// transcript, or any log. Render shows `•` per character with an optional
/// last-four reveal for confirmation.
pub(crate) struct SecretEntryState {
    /// Env var name the key is stored under in the keychain
    /// (e.g. `OPENAI_API_KEY`).
    pub env_var: String,
    /// Human-readable provider name for the header (e.g. "OpenAI").
    pub provider_label: String,
    pub draft: String,
    pub cursor: usize,
    /// When `true`, reveal the last four characters so the user can
    /// double-check what they pasted. Toggled with Ctrl+T.
    pub reveal_tail: bool,
}

/// Filterable picker driven by `squeezy_llm::registry::MODEL_REGISTRY`.
/// Opens when the user presses Enter on the `[model].model` field.
pub(crate) struct ModelPickerState {
    pub filter: String,
    pub cursor: usize,
    pub all_providers: bool,
    pub current_provider: &'static str,
}

/// Fuzzy search across every field label in `CONFIG_SECTIONS`. Triggered
/// by `/` in browse mode. Enter jumps to the matched field.
pub(crate) struct SearchOverlayState {
    pub query: String,
    pub cursor: usize,
    /// (section_index, field_index, score) for matches, sorted ascending
    /// by score (lower is better in `squeezy_rank::fuzzy::fuzzy_score`).
    pub matches: Vec<(usize, usize, i32)>,
}

/// Stand-alone editor state. Holds a draft buffer so cancel-on-Esc restores.
#[derive(Debug, Clone)]
pub(crate) enum FieldEditor {
    Text {
        draft: String,
        cursor: usize,
    },
    Integer {
        draft: String,
        cursor: usize,
        min: i64,
        max: i64,
    },
    OptionalInteger {
        draft: String,
        cursor: usize,
        min: i64,
        max: i64,
    },
    Enum {
        options: &'static [&'static str],
        cursor: usize,
    },
    OptionalEnum {
        options: &'static [&'static str],
        // 0 = unset, then options
        cursor: usize,
    },
    Bool(bool),
    Duration {
        draft: String,
        cursor: usize,
    },
    /// Comma-separated list editor — commits as `FieldValue::StringList`.
    /// Trailing/leading whitespace and empty items are trimmed.
    StringList {
        draft: String,
        cursor: usize,
    },
    /// Filesystem path editor — commits as `FieldValue::Path`.
    Path {
        draft: String,
        cursor: usize,
    },
}

impl ConfigScreenState {
    pub(crate) fn new(effective: AppConfig, focus: Option<SectionId>) -> Self {
        let sources =
            load_separated_settings_sources().unwrap_or_else(|_| empty_sources_for(&effective));
        let section_index = focus
            .and_then(|id| CONFIG_SECTIONS.iter().position(|s| s.id == id))
            .unwrap_or(0);
        Self {
            scope: ConfigScope::User,
            section_index,
            field_index: 0,
            editor: None,
            picker: None,
            search: None,
            secret_entry: None,
            effective,
            sources,
            dirty: false,
        }
    }

    pub(crate) fn current_section(&self) -> &'static ConfigSectionMeta {
        &CONFIG_SECTIONS[self.section_index]
    }

    pub(crate) fn current_field(&self) -> &'static FieldMeta {
        &self.current_section().fields[self.field_index]
    }

    pub(crate) fn field_source(&self, field: &FieldMeta) -> FieldSource {
        resolve_field_source(&self.sources, field)
    }
}

/// Outcome of a single key press on the screen. `Close` tells the host to
/// hide the screen; `KeepOpen` keeps it; `OpenedExternal` is reserved for
/// future "open shell" actions.
pub(crate) enum KeyOutcome {
    KeepOpen,
    Close,
}

pub(crate) fn handle_key(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut NotificationQueue,
    key: KeyEvent,
) -> KeyOutcome {
    // Sub-modes take precedence over the regular browse keymap.
    if state.secret_entry.is_some() {
        return handle_secret_entry_key(state, agent, notifications, key);
    }
    if state.search.is_some() {
        return handle_search_key(state, key);
    }
    if state.picker.is_some() {
        return handle_picker_key(state, agent, notifications, key);
    }
    if let Some(editor) = &mut state.editor {
        let commit = handle_editor_key(editor, key);
        match commit {
            EditorOutcome::KeepEditing => return KeyOutcome::KeepOpen,
            EditorOutcome::Cancel => {
                state.editor = None;
                return KeyOutcome::KeepOpen;
            }
            EditorOutcome::Commit(value) => {
                state.editor = None;
                let field = state.current_field();
                if let Err(msg) = (field.set)(&mut state.effective, value.clone()) {
                    notifications.push(format!("invalid: {msg}"), NotifySeverity::Error);
                } else {
                    state.dirty = true;
                    // Save immediately; the apply pipeline below routes the
                    // change to the right tier and queues notifications.
                    save_field(state, agent, notifications, field, value);
                }
                return KeyOutcome::KeepOpen;
            }
        }
    }

    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            if state.dirty {
                notifications.push(
                    "Closed with unsaved edits already applied. Re-open to view current state.",
                    NotifySeverity::Info,
                );
            }
            KeyOutcome::Close
        }
        (KeyCode::Tab, _) => {
            state.scope = match state.scope {
                ConfigScope::User => ConfigScope::Project,
                ConfigScope::Project => ConfigScope::User,
            };
            KeyOutcome::KeepOpen
        }
        (KeyCode::BackTab, _) => {
            state.scope = match state.scope {
                ConfigScope::User => ConfigScope::Project,
                ConfigScope::Project => ConfigScope::User,
            };
            KeyOutcome::KeepOpen
        }
        (KeyCode::Left, _) | (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
            if state.section_index == 0 {
                state.section_index = CONFIG_SECTIONS.len() - 1;
            } else {
                state.section_index -= 1;
            }
            state.field_index = 0;
            KeyOutcome::KeepOpen
        }
        (KeyCode::Right, _) | (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
            state.section_index = (state.section_index + 1) % CONFIG_SECTIONS.len();
            state.field_index = 0;
            KeyOutcome::KeepOpen
        }
        (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
            let n = state.current_section().fields.len();
            if state.field_index == 0 {
                state.field_index = n.saturating_sub(1);
            } else {
                state.field_index -= 1;
            }
            KeyOutcome::KeepOpen
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
            let n = state.current_section().fields.len();
            state.field_index = (state.field_index + 1) % n.max(1);
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char(' '), _) => {
            let field = state.current_field();
            // Env-shadowed fields are inert; show the same hint we use for
            // Enter so Space doesn't silently no-op.
            if let Some(var) = field.env_override
                && std::env::var(var).is_ok()
            {
                notifications.push(
                    format!(
                        "{} is set by {}; unset the env var to edit in the screen.",
                        field.label, var
                    ),
                    NotifySeverity::Warn,
                );
                return KeyOutcome::KeepOpen;
            }
            // Space cycles to the next value inline for any field where
            // "next" is well-defined: Bool toggles, Enum/OptionalEnum advance,
            // and the model field cycles through `squeezy_llm` registry
            // entries scoped to the current provider. Anything else surfaces
            // a notification so the user knows Space isn't a no-op by
            // accident.
            let current_value = (field.get)(&state.effective);
            let next: Option<FieldValue> = match (field.kind, &current_value) {
                (FieldKind::Bool, FieldValue::Bool(b)) => Some(FieldValue::Bool(!b)),
                (FieldKind::Enum { options }, FieldValue::Enum(current)) => {
                    let idx = options
                        .iter()
                        .position(|o| *o == *current)
                        .map(|i| (i + 1) % options.len())
                        .unwrap_or(0);
                    Some(FieldValue::Enum(options[idx]))
                }
                (FieldKind::OptionalEnum { options }, FieldValue::OptionalEnum(current)) => {
                    // None → options[0] → options[1] → ... → None
                    let next_idx = match current.and_then(|c| options.iter().position(|o| *o == c))
                    {
                        None => Some(0),
                        Some(i) if i + 1 < options.len() => Some(i + 1),
                        Some(_) => None,
                    };
                    Some(FieldValue::OptionalEnum(next_idx.map(|i| options[i])))
                }
                _ if field.toml_path == ["model", "model"] => {
                    cycle_to_next_registry_model(&state.effective, &current_value)
                }
                _ => None,
            };
            if let Some(next) = next {
                if (field.set)(&mut state.effective, next.clone()).is_ok() {
                    state.dirty = true;
                    save_field(state, agent, notifications, field, next);
                }
            } else {
                notifications.push(
                    format!(
                        "Space doesn't cycle {} — press Enter to edit.",
                        field.label
                    ),
                    NotifySeverity::Info,
                );
            }
            KeyOutcome::KeepOpen
        }
        (KeyCode::Enter, _) => {
            let field = state.current_field();
            // Refuse to edit env-shadowed fields — the value at runtime is
            // the env var's, not the TOML's, so a TOML write is silently
            // inert.
            if let Some(var) = field.env_override
                && std::env::var(var).is_ok()
            {
                notifications.push(
                    format!(
                        "{} is set by {}; unset the env var to edit in the screen.",
                        field.label, var
                    ),
                    NotifySeverity::Warn,
                );
                return KeyOutcome::KeepOpen;
            }
            // The model field opens a registry-driven picker; every other
            // field goes through the regular per-kind editor.
            if field.toml_path == ["model", "model"] {
                let current_provider = match (CONFIG_SECTIONS[0].fields[0].get)(&state.effective) {
                    FieldValue::Enum(s) => s,
                    _ => "openai",
                };
                state.picker = Some(ModelPickerState {
                    filter: String::new(),
                    cursor: 0,
                    all_providers: false,
                    current_provider,
                });
            } else if matches!(field.kind, FieldKind::Secret { .. }) {
                notifications.push(
                    "Use `squeezy auth set <provider>` to write the secret.",
                    NotifySeverity::Info,
                );
            } else if matches!(
                field.kind,
                FieldKind::TableArray { .. } | FieldKind::ProviderSubTabs
            ) {
                notifications.push(
                    format!(
                        "{} is not yet editable in the screen — edit the TOML directly for now.",
                        field.label
                    ),
                    NotifySeverity::Info,
                );
            } else {
                state.editor = Some(open_editor_for(field, (field.get)(&state.effective)));
            }
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('/'), m) if m.is_empty() => {
            state.search = Some(SearchOverlayState {
                query: String::new(),
                cursor: 0,
                matches: compute_search_matches(""),
            });
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
            let field = state.current_field();
            if let Some(var) = field.env_override
                && std::env::var(var).is_ok()
            {
                notifications.push(
                    format!(
                        "{} is set by {}; unset the env var to reset.",
                        field.label, var
                    ),
                    NotifySeverity::Warn,
                );
                return KeyOutcome::KeepOpen;
            }
            let default_val = (field.default)();
            if let Err(msg) = (field.set)(&mut state.effective, default_val.clone()) {
                notifications.push(format!("reset failed: {msg}"), NotifySeverity::Error);
            } else {
                state.dirty = true;
                save_field(state, agent, notifications, field, default_val);
            }
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
            // The current design saves on every commit (Enter / Space), so
            // Ctrl+S is a no-op affordance for muscle memory.
            notifications.push(
                "Changes auto-save on commit (Enter / Space).",
                NotifySeverity::Info,
            );
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
            if matches!(state.scope, ConfigScope::Project) {
                clear_project_override(state, notifications);
            } else {
                notifications.push(
                    "Ctrl+D clears overrides — switch to Project (Tab) first.",
                    NotifySeverity::Info,
                );
            }
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('K'), m) if m == KeyModifiers::SHIFT || m.is_empty() => {
            open_api_key_entry_for_current_provider(state, notifications);
            KeyOutcome::KeepOpen
        }
        _ => KeyOutcome::KeepOpen,
    }
}

#[derive(Debug)]
pub(crate) enum EditorOutcome {
    KeepEditing,
    Commit(FieldValue),
    Cancel,
}

fn open_editor_for(field: &FieldMeta, current: FieldValue) -> FieldEditor {
    match (field.kind, current) {
        (FieldKind::String { .. }, FieldValue::String(s)) => FieldEditor::Text {
            cursor: s.chars().count(),
            draft: s,
        },
        (FieldKind::String { .. }, _) => FieldEditor::Text {
            draft: String::new(),
            cursor: 0,
        },
        (FieldKind::Integer { min, max, .. }, FieldValue::Integer(v)) => FieldEditor::Integer {
            draft: v.to_string(),
            cursor: v.to_string().len(),
            min,
            max,
        },
        (FieldKind::Integer { min, max, .. }, _) => FieldEditor::Integer {
            draft: String::new(),
            cursor: 0,
            min,
            max,
        },
        (FieldKind::OptionalInteger { min, max, .. }, FieldValue::OptionalInteger(Some(v))) => {
            FieldEditor::OptionalInteger {
                draft: v.to_string(),
                cursor: v.to_string().len(),
                min,
                max,
            }
        }
        (FieldKind::OptionalInteger { min, max, .. }, _) => FieldEditor::OptionalInteger {
            draft: String::new(),
            cursor: 0,
            min,
            max,
        },
        (FieldKind::Enum { options }, FieldValue::Enum(v)) => {
            let cursor = options.iter().position(|o| *o == v).unwrap_or(0);
            FieldEditor::Enum { options, cursor }
        }
        (FieldKind::Enum { options }, _) => FieldEditor::Enum { options, cursor: 0 },
        (FieldKind::OptionalEnum { options }, FieldValue::OptionalEnum(Some(v))) => {
            let cursor = options
                .iter()
                .position(|o| *o == v)
                .map(|i| i + 1)
                .unwrap_or(0);
            FieldEditor::OptionalEnum { options, cursor }
        }
        (FieldKind::OptionalEnum { options }, _) => {
            FieldEditor::OptionalEnum { options, cursor: 0 }
        }
        (FieldKind::Bool, FieldValue::Bool(v)) => FieldEditor::Bool(v),
        (FieldKind::Bool, _) => FieldEditor::Bool(false),
        (FieldKind::DurationMs, FieldValue::Duration(d)) => FieldEditor::Duration {
            draft: d.as_millis().to_string(),
            cursor: d.as_millis().to_string().len(),
        },
        (FieldKind::DurationMs, _) => FieldEditor::Duration {
            draft: String::new(),
            cursor: 0,
        },
        (FieldKind::StringList { .. }, FieldValue::StringList(items)) => {
            let draft = items.join(", ");
            FieldEditor::StringList {
                cursor: draft.chars().count(),
                draft,
            }
        }
        (FieldKind::StringList { .. }, _) => FieldEditor::StringList {
            draft: String::new(),
            cursor: 0,
        },
        (FieldKind::Path { .. }, FieldValue::Path(p)) => {
            let draft = p.display().to_string();
            FieldEditor::Path {
                cursor: draft.chars().count(),
                draft,
            }
        }
        (FieldKind::Path { .. }, _) => FieldEditor::Path {
            draft: String::new(),
            cursor: 0,
        },
        // Secret / ProviderSubTabs / TableArray drop into dedicated sub-modes
        // (Secret entry, provider sub-tabs, table-array editor) in commit 5.
        // Until then, opening one is a no-op handled by `handle_key` — we
        // shouldn't have reached `open_editor_for` for these kinds.
        (
            FieldKind::Secret { .. } | FieldKind::ProviderSubTabs | FieldKind::TableArray { .. },
            _,
        ) => FieldEditor::Text {
            draft: String::new(),
            cursor: 0,
        },
    }
}

fn handle_editor_key(editor: &mut FieldEditor, key: KeyEvent) -> EditorOutcome {
    if key.code == KeyCode::Esc {
        return EditorOutcome::Cancel;
    }
    match editor {
        FieldEditor::Text { draft, cursor } => text_editor_key(draft, cursor, key, |d| {
            EditorOutcome::Commit(FieldValue::String(d.clone()))
        }),
        FieldEditor::Integer {
            draft,
            cursor,
            min,
            max,
        } => integer_editor_key(draft, cursor, *min, *max, key, false),
        FieldEditor::OptionalInteger {
            draft,
            cursor,
            min,
            max,
        } => integer_editor_key(draft, cursor, *min, *max, key, true),
        FieldEditor::Duration { draft, cursor } => {
            integer_editor_key(draft, cursor, 0, i64::MAX, key, false).map_value(|v| match v {
                FieldValue::Integer(ms) => {
                    FieldValue::Duration(std::time::Duration::from_millis(ms.max(0) as u64))
                }
                other => other,
            })
        }
        FieldEditor::Enum { options, cursor } => match key.code {
            KeyCode::Left | KeyCode::Up => {
                if *cursor == 0 {
                    *cursor = options.len() - 1;
                } else {
                    *cursor -= 1;
                }
                EditorOutcome::KeepEditing
            }
            KeyCode::Right | KeyCode::Down => {
                *cursor = (*cursor + 1) % options.len();
                EditorOutcome::KeepEditing
            }
            KeyCode::Enter => EditorOutcome::Commit(FieldValue::Enum(options[*cursor])),
            _ => EditorOutcome::KeepEditing,
        },
        FieldEditor::OptionalEnum { options, cursor } => match key.code {
            KeyCode::Left | KeyCode::Up => {
                if *cursor == 0 {
                    *cursor = options.len();
                } else {
                    *cursor -= 1;
                }
                EditorOutcome::KeepEditing
            }
            KeyCode::Right | KeyCode::Down => {
                *cursor = (*cursor + 1) % (options.len() + 1);
                EditorOutcome::KeepEditing
            }
            KeyCode::Enter => {
                let v = if *cursor == 0 {
                    FieldValue::OptionalEnum(None)
                } else {
                    FieldValue::OptionalEnum(Some(options[*cursor - 1]))
                };
                EditorOutcome::Commit(v)
            }
            _ => EditorOutcome::KeepEditing,
        },
        FieldEditor::Bool(v) => match key.code {
            KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right => {
                *v = !*v;
                EditorOutcome::KeepEditing
            }
            KeyCode::Enter => EditorOutcome::Commit(FieldValue::Bool(*v)),
            _ => EditorOutcome::KeepEditing,
        },
        FieldEditor::StringList { draft, cursor } => text_editor_key(draft, cursor, key, |d| {
            let items: Vec<String> = d
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            EditorOutcome::Commit(FieldValue::StringList(items))
        }),
        FieldEditor::Path { draft, cursor } => text_editor_key(draft, cursor, key, |d| {
            EditorOutcome::Commit(FieldValue::Path(std::path::PathBuf::from(d.trim())))
        }),
    }
}

trait MapValue {
    fn map_value<F>(self, f: F) -> EditorOutcome
    where
        F: FnOnce(FieldValue) -> FieldValue;
}

impl MapValue for EditorOutcome {
    fn map_value<F>(self, f: F) -> EditorOutcome
    where
        F: FnOnce(FieldValue) -> FieldValue,
    {
        match self {
            EditorOutcome::Commit(v) => EditorOutcome::Commit(f(v)),
            other => other,
        }
    }
}

fn text_editor_key<F>(
    draft: &mut String,
    cursor: &mut usize,
    key: KeyEvent,
    commit: F,
) -> EditorOutcome
where
    F: FnOnce(&String) -> EditorOutcome,
{
    match key.code {
        KeyCode::Enter => commit(draft),
        KeyCode::Char(c) => {
            let mut chars: Vec<char> = draft.chars().collect();
            chars.insert(*cursor, c);
            *draft = chars.into_iter().collect();
            *cursor += 1;
            EditorOutcome::KeepEditing
        }
        KeyCode::Backspace => {
            if *cursor > 0 {
                let mut chars: Vec<char> = draft.chars().collect();
                chars.remove(*cursor - 1);
                *draft = chars.into_iter().collect();
                *cursor -= 1;
            }
            EditorOutcome::KeepEditing
        }
        KeyCode::Left => {
            *cursor = cursor.saturating_sub(1);
            EditorOutcome::KeepEditing
        }
        KeyCode::Right => {
            *cursor = (*cursor + 1).min(draft.chars().count());
            EditorOutcome::KeepEditing
        }
        KeyCode::Home => {
            *cursor = 0;
            EditorOutcome::KeepEditing
        }
        KeyCode::End => {
            *cursor = draft.chars().count();
            EditorOutcome::KeepEditing
        }
        _ => EditorOutcome::KeepEditing,
    }
}

fn integer_editor_key(
    draft: &mut String,
    cursor: &mut usize,
    min: i64,
    max: i64,
    key: KeyEvent,
    optional: bool,
) -> EditorOutcome {
    match key.code {
        KeyCode::Enter => {
            if optional && draft.trim().is_empty() {
                return EditorOutcome::Commit(FieldValue::OptionalInteger(None));
            }
            match draft.trim().parse::<i64>() {
                Ok(v) if (min..=max).contains(&v) => {
                    if optional {
                        EditorOutcome::Commit(FieldValue::OptionalInteger(Some(v)))
                    } else {
                        EditorOutcome::Commit(FieldValue::Integer(v))
                    }
                }
                Ok(_) | Err(_) => EditorOutcome::KeepEditing,
            }
        }
        KeyCode::Char(c) if c.is_ascii_digit() || c == '-' => {
            let mut chars: Vec<char> = draft.chars().collect();
            chars.insert(*cursor, c);
            *draft = chars.into_iter().collect();
            *cursor += 1;
            EditorOutcome::KeepEditing
        }
        KeyCode::Backspace => {
            if *cursor > 0 {
                let mut chars: Vec<char> = draft.chars().collect();
                chars.remove(*cursor - 1);
                *draft = chars.into_iter().collect();
                *cursor -= 1;
            }
            EditorOutcome::KeepEditing
        }
        KeyCode::Left => {
            *cursor = cursor.saturating_sub(1);
            EditorOutcome::KeepEditing
        }
        KeyCode::Right => {
            *cursor = (*cursor + 1).min(draft.chars().count());
            EditorOutcome::KeepEditing
        }
        KeyCode::Home => {
            *cursor = 0;
            EditorOutcome::KeepEditing
        }
        KeyCode::End => {
            *cursor = draft.chars().count();
            EditorOutcome::KeepEditing
        }
        _ => EditorOutcome::KeepEditing,
    }
}

// ─── Save pipeline ───────────────────────────────────────────────────────────

// ─── Model picker ─────────────────────────────────────────────────────────────

/// Find the next model id from the `squeezy_llm` registry for the currently
/// configured provider. Returns `None` when the provider has no registry
/// entries (e.g. ollama on first run) so the caller can surface a hint
/// instead of silently no-op.
pub(crate) fn cycle_to_next_registry_model(
    effective: &AppConfig,
    current_value: &FieldValue,
) -> Option<FieldValue> {
    let provider = match (CONFIG_SECTIONS[0].fields[0].get)(effective) {
        FieldValue::Enum(s) => s,
        _ => "openai",
    };
    let models: Vec<&'static squeezy_llm::ModelInfo> =
        squeezy_llm::models_for_provider(provider).collect();
    if models.is_empty() {
        return None;
    }
    let current_id = match current_value {
        FieldValue::String(s) => s.as_str(),
        _ => "",
    };
    let next_idx = models
        .iter()
        .position(|m| m.id == current_id)
        .map(|i| (i + 1) % models.len())
        .unwrap_or(0);
    Some(FieldValue::String(models[next_idx].id.to_string()))
}

fn picker_matches(state: &ModelPickerState) -> Vec<&'static squeezy_llm::ModelInfo> {
    let filter_lower = state.filter.to_lowercase();
    squeezy_llm::MODEL_REGISTRY
        .iter()
        .filter(|m| state.all_providers || m.provider == state.current_provider)
        .filter(|m| filter_lower.is_empty() || m.id.to_lowercase().contains(&filter_lower))
        .collect()
}

fn handle_picker_key(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut NotificationQueue,
    key: KeyEvent,
) -> KeyOutcome {
    let picker = state.picker.as_mut().expect("checked by caller");
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            state.picker = None;
        }
        (KeyCode::Tab, _) => {
            picker.all_providers = !picker.all_providers;
            picker.cursor = 0;
        }
        (KeyCode::Up, _) => {
            let n = picker_matches(picker).len();
            if n > 0 && picker.cursor > 0 {
                picker.cursor -= 1;
            }
        }
        (KeyCode::Down, _) => {
            let n = picker_matches(picker).len();
            if n > 0 {
                picker.cursor = (picker.cursor + 1).min(n - 1);
            }
        }
        (KeyCode::Backspace, _) => {
            picker.filter.pop();
            picker.cursor = 0;
        }
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            picker.filter.push(c);
            picker.cursor = 0;
        }
        (KeyCode::Enter, m) if m.contains(KeyModifiers::CONTROL) => {
            // Ctrl+Enter — commit the raw filter buffer as a custom string.
            let custom = picker.filter.trim().to_string();
            if custom.is_empty() {
                notifications.push(
                    "Type a model id first, then Ctrl+Enter to commit.",
                    NotifySeverity::Info,
                );
            } else {
                commit_model_picker(state, agent, notifications, custom);
            }
        }
        (KeyCode::Enter, _) => {
            let matches = picker_matches(picker);
            if matches.is_empty() {
                notifications.push(
                    "No model matches the filter. Ctrl+Enter to commit the filter as a custom id.",
                    NotifySeverity::Info,
                );
            } else {
                let id = matches[picker.cursor.min(matches.len() - 1)].id.to_string();
                commit_model_picker(state, agent, notifications, id);
            }
        }
        _ => {}
    }
    KeyOutcome::KeepOpen
}

fn commit_model_picker(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut NotificationQueue,
    model_id: String,
) {
    state.picker = None;
    // The model field is the second field of the Models section. Look it up
    // by path rather than index in case the schema is reordered.
    let field = CONFIG_SECTIONS
        .iter()
        .flat_map(|s| s.fields.iter())
        .find(|f| f.toml_path == ["model", "model"])
        .expect("model field exists in CONFIG_SECTIONS");
    let value = FieldValue::String(model_id);
    if let Err(msg) = (field.set)(&mut state.effective, value.clone()) {
        notifications.push(format!("invalid: {msg}"), NotifySeverity::Error);
        return;
    }
    state.dirty = true;
    save_field(state, agent, notifications, field, value);
}

// ─── API key (Secret) entry ───────────────────────────────────────────────────

fn provider_api_key_env(provider: &squeezy_core::ProviderConfig) -> Option<(&'static str, String)> {
    use squeezy_core::ProviderConfig as P;
    match provider {
        P::OpenAi(c) => Some(("OpenAI", c.api_key_env.clone())),
        P::Anthropic(c) => Some(("Anthropic", c.api_key_env.clone())),
        P::Google(c) => Some(("Google", c.api_key_env.clone())),
        P::AzureOpenAi(c) => Some(("Azure OpenAI", c.api_key_env.clone())),
        // Bedrock uses AWS SDK creds; Ollama is local — neither has a single
        // env-var keychain entry the screen can write.
        P::Bedrock(_) | P::Ollama(_) => None,
    }
}

fn open_api_key_entry_for_current_provider(
    state: &mut ConfigScreenState,
    notifications: &mut NotificationQueue,
) {
    match provider_api_key_env(&state.effective.provider) {
        Some((label, env_var)) => {
            state.secret_entry = Some(SecretEntryState {
                env_var,
                provider_label: label.to_string(),
                draft: String::new(),
                cursor: 0,
                reveal_tail: false,
            });
        }
        None => {
            notifications.push(
                "This provider does not have a simple API-key env var \
                 (Bedrock uses AWS creds, Ollama is local).",
                NotifySeverity::Info,
            );
        }
    }
}

fn handle_secret_entry_key(
    state: &mut ConfigScreenState,
    _agent: &mut Agent,
    notifications: &mut NotificationQueue,
    key: KeyEvent,
) -> KeyOutcome {
    let entry = state.secret_entry.as_mut().expect("checked by caller");
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            // Wipe the draft so a peeker post-cancel can't read the
            // bytes off the heap.
            entry.draft.clear();
            entry.cursor = 0;
            state.secret_entry = None;
        }
        (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
            entry.reveal_tail = !entry.reveal_tail;
        }
        (KeyCode::Backspace, _) => {
            if entry.cursor > 0 {
                let mut chars: Vec<char> = entry.draft.chars().collect();
                chars.remove(entry.cursor - 1);
                entry.draft = chars.into_iter().collect();
                entry.cursor -= 1;
            }
        }
        (KeyCode::Left, _) => {
            entry.cursor = entry.cursor.saturating_sub(1);
        }
        (KeyCode::Right, _) => {
            entry.cursor = (entry.cursor + 1).min(entry.draft.chars().count());
        }
        (KeyCode::Home, _) => entry.cursor = 0,
        (KeyCode::End, _) => entry.cursor = entry.draft.chars().count(),
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            // Bracketed paste delivers each char individually through the
            // event loop, so this handles both interactive typing and
            // paste from the clipboard.
            let mut chars: Vec<char> = entry.draft.chars().collect();
            chars.insert(entry.cursor, c);
            entry.draft = chars.into_iter().collect();
            entry.cursor += 1;
        }
        (KeyCode::Enter, _) => {
            let env_var = entry.env_var.clone();
            let value = std::mem::take(&mut entry.draft);
            entry.cursor = 0;
            state.secret_entry = None;
            if value.trim().is_empty() {
                notifications.push(
                    "API key was empty — nothing written to the keychain.",
                    NotifySeverity::Warn,
                );
                return KeyOutcome::KeepOpen;
            }
            match squeezy_llm::save_api_key(&env_var, value.trim()) {
                Ok(()) => {
                    notifications.push(
                        format!(
                            "✓ saved {} to the OS keychain (not stored in any TOML)",
                            env_var
                        ),
                        NotifySeverity::Success,
                    );
                }
                Err(err) => {
                    notifications.push(
                        format!("failed to save {env_var}: {err}"),
                        NotifySeverity::Error,
                    );
                }
            }
        }
        _ => {}
    }
    KeyOutcome::KeepOpen
}

// ─── Search overlay ───────────────────────────────────────────────────────────

pub(crate) fn compute_search_matches(query: &str) -> Vec<(usize, usize, i32)> {
    let mut out: Vec<(usize, usize, i32)> = Vec::new();
    for (sidx, section) in CONFIG_SECTIONS.iter().enumerate() {
        for (fidx, field) in section.fields.iter().enumerate() {
            if query.is_empty() {
                out.push((sidx, fidx, 0));
                continue;
            }
            // Match against `<section>.<field>` so users can type either part.
            let target = format!("{} {}", section.label, field.label);
            if let Some(score) = squeezy_rank::fuzzy_score(&target, query) {
                out.push((sidx, fidx, score));
            }
        }
    }
    out.sort_by_key(|(_, _, score)| *score);
    out.truncate(40);
    out
}

fn handle_search_key(state: &mut ConfigScreenState, key: KeyEvent) -> KeyOutcome {
    let search = state.search.as_mut().expect("checked by caller");
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            state.search = None;
        }
        (KeyCode::Up, _) if !search.matches.is_empty() && search.cursor > 0 => {
            search.cursor -= 1;
        }
        (KeyCode::Down, _) => {
            let n = search.matches.len();
            if n > 0 {
                search.cursor = (search.cursor + 1).min(n - 1);
            }
        }
        (KeyCode::Backspace, _) => {
            search.query.pop();
            search.matches = compute_search_matches(&search.query);
            search.cursor = 0;
        }
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            search.query.push(c);
            search.matches = compute_search_matches(&search.query);
            search.cursor = 0;
        }
        (KeyCode::Enter, _) => {
            if let Some((sidx, fidx, _)) = search.matches.get(search.cursor).copied() {
                state.section_index = sidx;
                state.field_index = fidx;
            }
            state.search = None;
        }
        _ => {}
    }
    KeyOutcome::KeepOpen
}

fn save_field(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut NotificationQueue,
    field: &'static FieldMeta,
    value: FieldValue,
) {
    let scope = state.scope;
    let target_path = match scope {
        ConfigScope::User => state.sources.user_path_default.clone(),
        ConfigScope::Project => state.sources.project_path_default.clone(),
    };
    let scope_target = match scope {
        ConfigScope::User => SettingsScope::user(&target_path),
        ConfigScope::Project => SettingsScope::project(&target_path),
    };

    let edit = field_edit(field, &value);
    let outcome = match apply_edits(&scope_target, &[edit]) {
        Ok(o) => o,
        Err(err) => {
            notifications.push(
                format!("Failed to write {}: {err}", target_path.display()),
                NotifySeverity::Error,
            );
            return;
        }
    };

    // Refresh the inheritance map by reloading separated sources. If this
    // fails (e.g. some other tier's file became unreadable), we surface the
    // error via notification but keep the in-memory source stale — the next
    // open of the screen will re-read.
    match load_separated_settings_sources() {
        Ok(reloaded) => state.sources = reloaded,
        Err(err) => {
            notifications.push(
                format!("inheritance map stale: {err}"),
                NotifySeverity::Warn,
            );
        }
    }

    apply_by_tier(state, agent, notifications, field, &outcome);
}

fn apply_by_tier(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut NotificationQueue,
    field: &'static FieldMeta,
    outcome: &WriteOutcome,
) {
    let path_str = outcome.path.display().to_string();
    match field.tier {
        ApplyTier::Immediate => {
            agent.replace_config(state.effective.clone());
            notifications.push(
                format!("✓ saved {} to {}", field.label, path_str),
                NotifySeverity::Success,
            );
        }
        ApplyTier::NextPrompt => {
            // Edits to any `[model].*` field (and per-provider `[providers.*]`
            // fields, once we surface them) might require rebuilding the LLM
            // client — different api_key_env, base_url, or whole provider
            // variant. Build the new provider eagerly so the next turn
            // doesn't blow up with "missing OPENAI_API_KEY" on a fresh
            // anthropic swap.
            let touches_provider = field
                .toml_path
                .first()
                .copied()
                .map(|head| head == "model" || head == "providers")
                .unwrap_or(false);
            let new_provider = if touches_provider {
                match squeezy_llm::provider_from_config(&state.effective.provider) {
                    Ok(p) => Some(p),
                    Err(err) => {
                        notifications.push(
                            format!(
                                "saved to {} but the new provider failed to build: {err}",
                                path_str
                            ),
                            NotifySeverity::Error,
                        );
                        None
                    }
                }
            } else {
                None
            };
            agent.arm_config_swap(PendingConfigSwap {
                config: state.effective.clone(),
                provider: new_provider,
                display_note: Some(format!("{} changed", field.label)),
            });
            notifications.push(
                format!(
                    "{} changed — applies on next prompt. Saved to {}",
                    field.label, path_str
                ),
                NotifySeverity::Info,
            );
        }
        ApplyTier::Restart => {
            notifications.push(
                format!(
                    "{} saved to {}. Restart required for the change to take effect.",
                    field.label, path_str
                ),
                NotifySeverity::Warn,
            );
        }
    }
}

fn field_edit(field: &'static FieldMeta, value: &FieldValue) -> SettingsEdit {
    let op = match (field.kind, value) {
        (_, FieldValue::Unset) => EditOp::Unset,
        (_, FieldValue::String(s)) => EditOp::SetString(s.clone()),
        (_, FieldValue::Bool(b)) => EditOp::SetBool(*b),
        (_, FieldValue::Integer(v)) => EditOp::SetInteger(*v),
        (_, FieldValue::OptionalInteger(Some(v))) => EditOp::SetInteger(*v),
        (_, FieldValue::OptionalInteger(None)) => EditOp::Unset,
        (_, FieldValue::Enum(s)) => EditOp::SetString((*s).to_string()),
        (_, FieldValue::OptionalEnum(Some(s))) => EditOp::SetString((*s).to_string()),
        (_, FieldValue::OptionalEnum(None)) => EditOp::Unset,
        (_, FieldValue::Duration(d)) => EditOp::SetInteger(d.as_millis() as i64),
        (_, FieldValue::StringList(items)) => EditOp::SetArrayOfStrings(items.clone()),
        (_, FieldValue::Path(p)) => EditOp::SetString(p.display().to_string()),
        // Secret / SubTabs / TableArray* never go through field_edit; their
        // commits are routed to dedicated handlers in commit 5. If we ever
        // reach this arm it's a programmer bug.
        (_, FieldValue::Secret)
        | (_, FieldValue::SubTabs(_))
        | (_, FieldValue::TableArrayKeyed(_))
        | (_, FieldValue::TableArrayOrdered(_)) => EditOp::Unset,
    };
    SettingsEdit {
        path: field.toml_path,
        op,
    }
}

fn clear_project_override(state: &mut ConfigScreenState, notifications: &mut NotificationQueue) {
    let field = state.current_field();
    let path = state.sources.project_path_default.clone();
    let scope_target = SettingsScope::project(&path);
    let edit = SettingsEdit {
        path: field.toml_path,
        op: EditOp::Unset,
    };
    match apply_edits(&scope_target, &[edit]) {
        Ok(outcome) if outcome.edits_applied > 0 => {
            if let Ok(reloaded) = load_separated_settings_sources() {
                state.sources = reloaded;
            }
            notifications.push(
                format!(
                    "cleared {} override in {} (now inherited)",
                    field.label,
                    path.display()
                ),
                NotifySeverity::Success,
            );
        }
        Ok(_) => {
            notifications.push(
                format!("{} had no project override to clear", field.label),
                NotifySeverity::Info,
            );
        }
        Err(err) => {
            notifications.push(
                format!("Failed to clear override: {err}"),
                NotifySeverity::Error,
            );
        }
    }
}

// ─── Rendering ────────────────────────────────────────────────────────────────

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
    let user_active = state.scope == ConfigScope::User;
    let project_active = state.scope == ConfigScope::Project;
    let title = Span::styled(
        "  Config  ",
        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
    );
    let sep1 = Span::styled(" │ ", Style::default().fg(QUIET));
    let sep2 = Span::styled(" │ ", Style::default().fg(QUIET));
    let sep3 = Span::styled(" │ ", Style::default().fg(QUIET));
    let user = Span::styled(
        if user_active {
            "▸ User (~/.squeezy/settings.toml)"
        } else {
            "  User (~/.squeezy/settings.toml)"
        },
        Style::default()
            .fg(if user_active { GOLD } else { Color::White })
            .add_modifier(if user_active {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
    );
    let project = Span::styled(
        if project_active {
            "▸ Project (./squeezy.toml, committed to repo)"
        } else {
            "  Project (./squeezy.toml, committed to repo)"
        },
        Style::default()
            .fg(if project_active { GOLD } else { Color::White })
            .add_modifier(if project_active {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
    );
    let dirty_str = if state.dirty {
        Span::styled("  (changes applied)", Style::default().fg(QUIET))
    } else {
        Span::raw("")
    };
    let _ = sep3;
    let line = Line::from(vec![title, sep1, user, sep2, project, dirty_str]);
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(QUIET));
    frame.render_widget(Paragraph::new(line).block(block), area);
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
    if let Some(entry) = &state.secret_entry {
        render_secret_entry(frame, chunks[2], entry);
    } else if let Some(picker) = &state.picker {
        render_model_picker(frame, chunks[2], picker);
    } else if let Some(search) = &state.search {
        render_search_overlay(frame, chunks[2], search);
    } else {
        render_field_pane(frame, chunks[2], state);
    }
}

fn render_secret_entry(frame: &mut Frame<'_>, area: Rect, entry: &SecretEntryState) {
    let chars: Vec<char> = entry.draft.chars().collect();
    let total = chars.len();
    let tail_visible = if entry.reveal_tail { total.min(4) } else { 0 };
    let masked_count = total - tail_visible;
    let mut masked = String::with_capacity(total);
    for _ in 0..masked_count {
        masked.push('•');
    }
    for c in &chars[masked_count..] {
        masked.push(*c);
    }
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
            Span::styled(masked, Style::default().fg(Color::White)),
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
                if entry.reveal_tail {
                    "hide tail  "
                } else {
                    "reveal last 4 chars  "
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
    frame.render_widget(Paragraph::new(lines), area);
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

    let max_label = section
        .fields
        .iter()
        .map(|f| f.label.len())
        .max()
        .unwrap_or(0);

    // When an editor is open, focus the pane on just the active field + the
    // editor block, so the editor is always visible in small viewports.
    // Otherwise list every field.
    let editing = state.editor.is_some();
    let fields_iter: Box<dyn Iterator<Item = (usize, &'static FieldMeta)>> = if editing {
        let idx = state.field_index;
        Box::new(std::iter::once((idx, &section.fields[idx])))
    } else {
        Box::new(section.fields.iter().enumerate())
    };

    for (idx, field) in fields_iter {
        let active = idx == state.field_index;
        let value = (field.get)(&state.effective);
        let value_str = value.as_display();
        let source = state.field_source(field);
        let source_label = format!("[{}]", source.badge());
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
            Span::styled(
                format!("{:<width$}", field.label, width = max_label + 2),
                style,
            ),
            Span::styled(
                value_str,
                Style::default().fg(if active { GOLD } else { Color::White }),
            ),
            Span::raw(" "),
            Span::styled(source_label, source_style(source)),
        ]));
    }

    lines.push(Line::raw(""));
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
        Span::styled("K", Style::default().fg(GOLD)),
        Span::raw(" set API key · "),
        Span::styled("/", Style::default().fg(GOLD)),
        Span::raw(" search · "),
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

fn empty_sources_for(_cfg: &AppConfig) -> SeparatedSources {
    SeparatedSources {
        user: None,
        project: None,
        repo: None,
        user_path_default: PathBuf::from(""),
        project_path_default: PathBuf::from(""),
    }
}

#[cfg(test)]
#[path = "config_screen_tests.rs"]
mod tests;
