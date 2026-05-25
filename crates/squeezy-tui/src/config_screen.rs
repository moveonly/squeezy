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
    load_separated_settings_sources,
    settings_writer::{EditOp, SettingsEdit, SettingsScope, WriteOutcome, apply_edits},
};

use crate::{
    notification::{NotificationQueue, Severity as NotifySeverity},
    render::palette::{AMBER, ERROR_RED, GOLD, MODE_PURPLE, QUIET, SUCCESS_GREEN},
};

/// Synthetic row index in the Models section that exposes the API-key
/// editor for the currently selected provider. Sits right after `model`
/// so provider + model + key read top-to-bottom as a single "what model
/// am I talking to and with which credential" cluster. Not backed by a
/// `FieldMeta` in `CONFIG_SECTIONS`.
const SYNTHETIC_KEY_ROW: usize = 2;

/// Static row metadata for the synthetic Reset section. Each row deletes
/// one tier's TOML file. The `Reset` section itself is declared in
/// `CONFIG_SECTIONS` with an empty `fields` slice — the rendering and
/// key handling consult this table instead.
const RESET_ACTIONS: &[ResetAction] = &[
    ResetAction {
        scope: ConfigScope::User,
        label: "Reset User settings",
        detail: "delete ~/.squeezy/settings.toml — every tab falls back to the binary defaults.",
    },
    ResetAction {
        scope: ConfigScope::Repo,
        label: "Reset Repo settings",
        detail: "delete ./squeezy.toml (committed) — Repo and Local tabs inherit User / defaults again.",
    },
    ResetAction {
        scope: ConfigScope::Local,
        label: "Reset Local settings",
        detail: "delete ~/.squeezy/projects/<this>/settings.toml — Local tab inherits Repo / User / defaults again.",
    },
];

/// A single row in the Reset section. Deletes the corresponding tier
/// file after a `y/n` confirmation. Inherited values from the remaining
/// tiers then take over — no other tab's file is touched.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResetAction {
    scope: ConfigScope,
    label: &'static str,
    detail: &'static str,
}

/// Three scope tabs surfaced in the screen, ordered low → high precedence.
///
/// Reminder of the internal-to-UI mapping (the names diverge for historical
/// reasons — `squeezy-core`'s tiers are user / project / repo):
///
///   User  → `~/.squeezy/settings.toml`                            (user)
///   Repo  → `./squeezy.toml`, committed to the repo               (project)
///   Local → `~/.squeezy/projects/<hash>/settings.toml`, per-machine (repo)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigScope {
    User,
    Repo,
    Local,
}

impl ConfigScope {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Repo => "Repo",
            Self::Local => "Local",
        }
    }

    pub(crate) fn next(self) -> Self {
        match self {
            Self::User => Self::Repo,
            Self::Repo => Self::Local,
            Self::Local => Self::User,
        }
    }

    pub(crate) fn prev(self) -> Self {
        match self {
            Self::User => Self::Local,
            Self::Repo => Self::User,
            Self::Local => Self::Repo,
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
    /// Pending tier-file deletion awaiting `y/n` confirmation. Set when the
    /// user presses Enter on a Reset-section row; cleared by `y` (after the
    /// delete fires) or `n` / Esc (cancel).
    pub reset_confirm: Option<ConfigScope>,
    pub effective: AppConfig,
    pub sources: SeparatedSources,
    pub dirty: bool,
    /// File bytes captured the moment the screen opened, per tier path.
    /// `Discard all` rewrites every file to its baseline.
    pub baseline: Vec<(std::path::PathBuf, Option<Vec<u8>>)>,
    /// `(path, pre_write_bytes)` for every write since the screen opened.
    /// `Ctrl+Z` pops the last entry and rewrites the file to its
    /// pre-write contents.
    pub undo_stack: Vec<(std::path::PathBuf, Option<Vec<u8>>)>,
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
    /// Cursor position in chars (not bytes). Stays valid across multibyte
    /// pastes; converted to a byte index on the fly when we need to mutate
    /// `draft`.
    pub cursor: usize,
    /// When `true`, render the full plaintext key for sanity-checking
    /// what was pasted. Toggled explicitly with Ctrl+T — both the toggle
    /// and the disclosure are the user's own action, so reveal in full
    /// rather than hiding the suffix.
    pub reveal: bool,
}

impl SecretEntryState {
    /// Number of characters in `draft`. Used for the cursor bound and the
    /// render mask width.
    fn char_len(&self) -> usize {
        self.draft.chars().count()
    }

    /// Map a char-index cursor to a byte position usable by
    /// `String::insert` / `String::remove`. Returns `None` when the index
    /// sits past the end of the string.
    fn char_to_byte(&self, char_idx: usize) -> Option<usize> {
        if char_idx == self.char_len() {
            return Some(self.draft.len());
        }
        self.draft
            .char_indices()
            .nth(char_idx)
            .map(|(byte_idx, _)| byte_idx)
    }

    /// Insert `c` at the current cursor and advance one char to the right.
    /// Used by interactive typing and by bracketed-paste delivery, which
    /// arrives as a stream of single-char `KeyEvent::Char` events.
    fn insert_char(&mut self, c: char) {
        if let Some(byte_idx) = self.char_to_byte(self.cursor) {
            self.draft.insert(byte_idx, c);
            self.cursor += 1;
        }
    }

    /// Delete the char immediately to the left of the cursor (backspace).
    /// No-op when the cursor is already at the start.
    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        if let Some(byte_idx) = self.char_to_byte(self.cursor - 1) {
            self.draft.remove(byte_idx);
            self.cursor -= 1;
        }
    }

    /// Zero out the in-memory plaintext. Called on Esc / Enter to keep a
    /// post-cancel peeker from reading the bytes off the heap.
    fn wipe(&mut self) {
        self.draft.clear();
        self.cursor = 0;
    }
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
        // Snapshot every tier file's bytes the moment the screen opens.
        // Discard-all rewrites these back; the undo stack covers the
        // finer-grained per-save history.
        let baseline = vec![
            (
                sources.user_path_default.clone(),
                std::fs::read(&sources.user_path_default).ok(),
            ),
            (
                sources.project_path_default.clone(),
                std::fs::read(&sources.project_path_default).ok(),
            ),
            (
                sources.repo_path_default.clone(),
                std::fs::read(&sources.repo_path_default).ok(),
            ),
        ];
        Self {
            scope: ConfigScope::User,
            section_index,
            field_index: 0,
            editor: None,
            picker: None,
            search: None,
            secret_entry: None,
            reset_confirm: None,
            effective,
            sources,
            dirty: false,
            baseline,
            undo_stack: Vec::new(),
        }
    }

    pub(crate) fn current_section(&self) -> &'static ConfigSectionMeta {
        &CONFIG_SECTIONS[self.section_index]
    }

    /// Real field at the focused row. Panics when the focus lands on the
    /// synthetic API-key row — callers must check `on_synthetic_api_key_row`
    /// first.
    pub(crate) fn current_field(&self) -> &'static FieldMeta {
        self.field_at_row(self.field_index)
            .expect("caller should branch on on_synthetic_api_key_row first")
    }

    /// Compute the displayed value + source for `field` under the currently
    /// active scope tab. Walks the precedence chain DOWN from the active
    /// tier (e.g. on the Local tab: Local → Repo → User → defaults) so
    /// editing the User file doesn't make the Local tab appear to also
    /// have that value — it shows `[inherited-user]` instead.
    /// `true` when the focus is on the synthetic "API key" row that
    /// sits right after `[model].provider`. The row has no `FieldMeta`
    /// in `CONFIG_SECTIONS` — it's a UI affordance keyed off the
    /// currently-selected provider's `api_key_env`.
    pub(crate) fn on_synthetic_api_key_row(&self) -> bool {
        self.current_section().id == SectionId::Models && self.field_index == SYNTHETIC_KEY_ROW
    }

    /// Number of selectable rows on the active section, including the
    /// synthetic "API key" row for the Models section and the three
    /// per-tier action rows for the Reset section.
    pub(crate) fn row_count(&self) -> usize {
        let section = self.current_section();
        match section.id {
            SectionId::Models => section.fields.len() + 1,
            SectionId::Reset => RESET_ACTIONS.len(),
            _ => section.fields.len(),
        }
    }

    /// Map a row index back to a real `FieldMeta` — `None` for the
    /// synthetic API-key row and for every row in the Reset section.
    ///
    /// Models layout, top to bottom: `provider` → `model` → synthetic
    /// `api_key` → the rest of the fields. The api_key row pretends to
    /// be the field at `SYNTHETIC_KEY_ROW`, so every row above it indexes
    /// `fields[row]` and every row below it indexes `fields[row - 1]`.
    pub(crate) fn field_at_row(&self, row: usize) -> Option<&'static FieldMeta> {
        let section = self.current_section();
        match section.id {
            SectionId::Models => match row {
                SYNTHETIC_KEY_ROW => None,
                r if r < SYNTHETIC_KEY_ROW => section.fields.get(r),
                r => section.fields.get(r - 1),
            },
            SectionId::Reset => None,
            _ => section.fields.get(row),
        }
    }

    /// Reset action for the focused row when the active section is `Reset`.
    pub(crate) fn reset_action_at_row(&self, row: usize) -> Option<&'static ResetAction> {
        if self.current_section().id == SectionId::Reset {
            RESET_ACTIONS.get(row)
        } else {
            None
        }
    }

    /// `true` when the focus is on a Reset-section action row.
    pub(crate) fn on_reset_action_row(&self) -> bool {
        self.current_section().id == SectionId::Reset
    }

    /// Whether the active scope's tier file explicitly sets the field.
    /// `None` means we don't have a tier source loaded for this scope
    /// (the file is missing on disk). Used by Space-cycle to decide
    /// between "start owning" and "advance / clear".
    pub(crate) fn scope_owns_field(&self, field: &FieldMeta) -> Option<bool> {
        let tier = match self.scope {
            ConfigScope::User => self.sources.user.as_ref(),
            ConfigScope::Repo => self.sources.project.as_ref(),
            ConfigScope::Local => self.sources.repo.as_ref(),
        }?;
        Some(tier.contains_path(field.toml_path))
    }

    pub(crate) fn displayed_value_and_source(
        &self,
        field: &FieldMeta,
    ) -> (FieldValue, FieldSource) {
        // env always wins — render the running value with [env] regardless of tab.
        if let Some(var) = field.env_override
            && std::env::var(var).is_ok()
        {
            return ((field.get)(&self.effective), FieldSource::Env);
        }
        // Precedence chain for the active tab, highest → lowest.
        let chain: &[(FieldSource, &Option<squeezy_core::TierSource>)] = match self.scope {
            ConfigScope::User => &[(FieldSource::User, &self.sources.user)],
            ConfigScope::Repo => &[
                (FieldSource::Project, &self.sources.project),
                (FieldSource::User, &self.sources.user),
            ],
            ConfigScope::Local => &[
                (FieldSource::Repo, &self.sources.repo),
                (FieldSource::Project, &self.sources.project),
                (FieldSource::User, &self.sources.user),
            ],
        };
        for (src, tier) in chain {
            if let Some(t) = tier
                && let Some(val) = tier_value_at_path(t, field)
            {
                return (val, *src);
            }
        }
        ((field.default)(), FieldSource::Default)
    }
}

/// Parse the `FieldValue` for `field.toml_path` out of a tier's
/// `DocumentMut`. Returns `None` when the path is unset in this tier or
/// when the leaf type can't be represented in the current schema (e.g.
/// `TableArray` / `ProviderSubTabs`).
fn tier_value_at_path(tier: &squeezy_core::TierSource, field: &FieldMeta) -> Option<FieldValue> {
    if field.toml_path.is_empty() {
        return None;
    }
    let (leaf, parents) = field.toml_path.split_last().unwrap();
    let mut node: &toml_edit::Item = tier.doc.as_item();
    for seg in parents {
        node = match node {
            toml_edit::Item::Table(t) => t.get(seg)?,
            toml_edit::Item::Value(toml_edit::Value::InlineTable(it)) => {
                let value = it.get(seg)?;
                // Borrow extension dance: wrap back as a temporary Item.
                // We need an `&Item` to keep walking; InlineTable values are
                // `&Value` so we synthesize one via `as_value`. Simpler: bail
                // on inline-table parents — they're rare in user files.
                let _ = value;
                return None;
            }
            _ => return None,
        };
    }
    let item = match node {
        toml_edit::Item::Table(t) => t.get(leaf)?,
        _ => return None,
    };
    let value = item.as_value()?;
    match field.kind {
        FieldKind::Bool => value.as_bool().map(FieldValue::Bool),
        FieldKind::Integer { .. } => value.as_integer().map(FieldValue::Integer),
        FieldKind::OptionalInteger { .. } => value
            .as_integer()
            .map(|v| FieldValue::OptionalInteger(Some(v))),
        FieldKind::Enum { options } => value
            .as_str()
            .and_then(|s| options.iter().find(|o| **o == s).copied())
            .map(FieldValue::Enum),
        FieldKind::OptionalEnum { options } => value
            .as_str()
            .and_then(|s| options.iter().find(|o| **o == s).copied())
            .map(|s| FieldValue::OptionalEnum(Some(s))),
        FieldKind::String { .. } => value.as_str().map(|s| FieldValue::String(s.to_string())),
        FieldKind::DurationMs => value
            .as_integer()
            .map(|ms| FieldValue::Duration(std::time::Duration::from_millis(ms.max(0) as u64))),
        FieldKind::StringList { .. } => {
            let arr = value.as_array()?;
            let mut items = Vec::with_capacity(arr.len());
            for v in arr.iter() {
                items.push(v.as_str()?.to_string());
            }
            Some(FieldValue::StringList(items))
        }
        FieldKind::Path { .. } => value
            .as_str()
            .map(|s| FieldValue::Path(std::path::PathBuf::from(s))),
        FieldKind::Secret { .. } | FieldKind::ProviderSubTabs | FieldKind::TableArray { .. } => {
            None
        }
    }
}

/// Inheritance badge label shown next to the field's value.
///
/// The vocabulary is intentionally uniform across tabs:
///   - `[env]`                — overridden by an environment variable.
///   - `[inherited-default]`  — falls through to the binary defaults.
///   - `[inherited-<tier>]`   — falls through to a higher-precedence tier.
///   - bare tier name         — the value lives in the active tab's file.
///
/// Treating the binary defaults as `[inherited-default]` on every tab
/// (including User) keeps the labelling honest: the User tab does not own
/// a value that nobody has typed, so it shouldn't read as `[default]` as
/// if it were a deliberate setting.
fn inheritance_label(active: ConfigScope, source: FieldSource) -> String {
    if source == FieldSource::Env {
        return "[env]".to_string();
    }
    if source == FieldSource::Default {
        return "[inherited-default]".to_string();
    }
    let scope_of_source = match source {
        FieldSource::User => ConfigScope::User,
        FieldSource::Project => ConfigScope::Repo,
        FieldSource::Repo => ConfigScope::Local,
        FieldSource::Env | FieldSource::Default => unreachable!(),
    };
    if scope_of_source == active {
        active.label().to_lowercase()
    } else {
        format!("[inherited-{}]", scope_of_source.label().to_lowercase())
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
    if state.reset_confirm.is_some() {
        return handle_reset_confirm_key(state, agent, notifications, key);
    }
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
            state.scope = state.scope.next();
            KeyOutcome::KeepOpen
        }
        (KeyCode::BackTab, _) => {
            state.scope = state.scope.prev();
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
            let n = state.row_count();
            if state.field_index == 0 {
                state.field_index = n.saturating_sub(1);
            } else {
                state.field_index -= 1;
            }
            KeyOutcome::KeepOpen
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
            let n = state.row_count();
            state.field_index = (state.field_index + 1) % n.max(1);
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char(' '), _) => {
            if state.on_synthetic_api_key_row() {
                open_api_key_entry_for_current_provider(state, notifications);
                return KeyOutcome::KeepOpen;
            }
            if let Some(action) = state.reset_action_at_row(state.field_index) {
                state.reset_confirm = Some(action.scope);
                return KeyOutcome::KeepOpen;
            }
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
            // Space cycles inline through the field's value space. On the
            // Repo and Local tabs the cycle includes a virtual "inherit"
            // position so the user can return to the parent tier's value
            // without leaving the row.
            let current_value = (field.get)(&state.effective);
            let active_owns_field = state.scope_owns_field(field).unwrap_or(false);
            // On non-User tabs that aren't currently owning the field, a
            // single Space starts owning it with the FIRST option. The
            // next Spaces walk through the options; when we reach the end
            // we clear the override (return to inheriting from the parent
            // tier) and the next Space starts the cycle over.
            if matches!(state.scope, ConfigScope::Repo | ConfigScope::Local) {
                let inherit_action = !active_owns_field;
                let mut should_clear = false;
                let next_value: Option<FieldValue> = match (field.kind, &current_value) {
                    _ if inherit_action => {
                        // Currently inherited — first Space starts owning at
                        // the first cyclable value (or toggles for Bool).
                        match (field.kind, &current_value) {
                            (FieldKind::Bool, FieldValue::Bool(b)) => Some(FieldValue::Bool(!b)),
                            (FieldKind::Enum { options }, _) => Some(FieldValue::Enum(options[0])),
                            (FieldKind::OptionalEnum { options }, _) => {
                                Some(FieldValue::OptionalEnum(Some(options[0])))
                            }
                            _ if field.toml_path == ["model", "model"] => {
                                cycle_to_next_registry_model(&state.effective, &current_value)
                            }
                            _ => None,
                        }
                    }
                    (FieldKind::Bool, FieldValue::Bool(true)) => {
                        // Last position of Bool — wrap to "inherit" by
                        // clearing the override.
                        should_clear = true;
                        None
                    }
                    (FieldKind::Bool, FieldValue::Bool(false)) => Some(FieldValue::Bool(true)),
                    (FieldKind::Enum { options }, FieldValue::Enum(current)) => {
                        let idx = options.iter().position(|o| *o == *current).unwrap_or(0);
                        if idx + 1 >= options.len() {
                            should_clear = true;
                            None
                        } else {
                            Some(FieldValue::Enum(options[idx + 1]))
                        }
                    }
                    (FieldKind::OptionalEnum { options }, FieldValue::OptionalEnum(current)) => {
                        let next_idx =
                            match current.and_then(|c| options.iter().position(|o| *o == c)) {
                                None => Some(0),
                                Some(i) if i + 1 < options.len() => Some(i + 1),
                                Some(_) => None,
                            };
                        match next_idx {
                            Some(i) => Some(FieldValue::OptionalEnum(Some(options[i]))),
                            None => {
                                should_clear = true;
                                None
                            }
                        }
                    }
                    _ if field.toml_path == ["model", "model"] => {
                        cycle_to_next_registry_model(&state.effective, &current_value)
                    }
                    _ => None,
                };
                if should_clear {
                    clear_scope_override_silent(state, notifications);
                    return KeyOutcome::KeepOpen;
                }
                if let Some(next) = next_value {
                    if (field.set)(&mut state.effective, next.clone()).is_ok() {
                        state.dirty = true;
                        save_field_silent(state, agent, notifications, field, next);
                    }
                    return KeyOutcome::KeepOpen;
                }
                notifications.push(
                    format!("Space doesn't cycle {} — press Enter to edit.", field.label),
                    NotifySeverity::Info,
                );
                return KeyOutcome::KeepOpen;
            }

            // User tab: cycle through values without an inherit position.
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
                    save_field_silent(state, agent, notifications, field, next);
                }
            } else {
                notifications.push(
                    format!("Space doesn't cycle {} — press Enter to edit.", field.label),
                    NotifySeverity::Info,
                );
            }
            KeyOutcome::KeepOpen
        }
        (KeyCode::Enter, _) => {
            if state.on_synthetic_api_key_row() {
                open_api_key_entry_for_current_provider(state, notifications);
                return KeyOutcome::KeepOpen;
            }
            if let Some(action) = state.reset_action_at_row(state.field_index) {
                state.reset_confirm = Some(action.scope);
                return KeyOutcome::KeepOpen;
            }
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
            if state.on_synthetic_api_key_row() {
                notifications.push(
                    "API key has no default — use Enter / Space here to set, or clear it from the OS keychain manually.",
                    NotifySeverity::Info,
                );
                return KeyOutcome::KeepOpen;
            }
            if state.on_reset_action_row() {
                notifications.push(
                    "Use Enter on the Reset row (with y/n confirm) to delete a tier file.",
                    NotifySeverity::Info,
                );
                return KeyOutcome::KeepOpen;
            }
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
            if state.on_synthetic_api_key_row() || state.on_reset_action_row() {
                return KeyOutcome::KeepOpen;
            }
            match state.scope {
                ConfigScope::User => {
                    notifications.push(
                        "Ctrl+D clears Repo/Local overrides — switch to Repo or Local (Tab) first.",
                        NotifySeverity::Info,
                    );
                }
                ConfigScope::Repo | ConfigScope::Local => {
                    clear_scope_override(state, notifications);
                }
            }
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('z'), KeyModifiers::CONTROL) => {
            undo_last_write(state, agent, notifications);
            KeyOutcome::KeepOpen
        }
        (KeyCode::Char('X'), m) if m == KeyModifiers::SHIFT || m.is_empty() => {
            discard_all_session_writes(state, agent, notifications);
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

// ─── Reset tab (tier-file deletion) ──────────────────────────────────────────

/// Resolve the tier file path for `scope` using the currently-loaded
/// `SeparatedSources`. The `*_path_default` fields hold the canonical
/// location even when the file does not exist on disk.
fn tier_path(state: &ConfigScreenState, scope: ConfigScope) -> std::path::PathBuf {
    match scope {
        ConfigScope::User => state.sources.user_path_default.clone(),
        ConfigScope::Repo => state.sources.project_path_default.clone(),
        ConfigScope::Local => state.sources.repo_path_default.clone(),
    }
}

fn handle_reset_confirm_key(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut NotificationQueue,
    key: KeyEvent,
) -> KeyOutcome {
    let scope = state.reset_confirm.expect("checked by caller");
    match (key.code, key.modifiers) {
        (KeyCode::Char('y'), _) | (KeyCode::Char('Y'), _) => {
            state.reset_confirm = None;
            perform_reset(state, agent, notifications, scope);
        }
        (KeyCode::Esc, _) | (KeyCode::Char('n'), _) | (KeyCode::Char('N'), _) => {
            state.reset_confirm = None;
            notifications.push("Reset cancelled.", NotifySeverity::Info);
        }
        _ => {}
    }
    KeyOutcome::KeepOpen
}

/// Delete `scope`'s tier file (no-op when it doesn't exist on disk) and
/// reload the in-memory sources + effective config. Any field whose
/// previous value lived in the deleted file falls back through the
/// remaining tiers — that recompute can also change values shown on
/// other tabs, which is exactly what "reset" promises.
fn perform_reset(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut NotificationQueue,
    scope: ConfigScope,
) {
    let path = tier_path(state, scope);
    if path.as_os_str().is_empty() {
        notifications.push(
            format!(
                "{} tier has no resolved file path; nothing to reset.",
                scope.label()
            ),
            NotifySeverity::Warn,
        );
        return;
    }
    // Snapshot the file bytes so Ctrl+Z can put them back.
    let pre = std::fs::read(&path).ok();
    state.undo_stack.push((path.clone(), pre.clone()));
    match restore_path(&path, None) {
        Ok(()) => {
            reload_sources_and_agent(state, agent, notifications);
            let msg = if pre.is_some() {
                format!(
                    "✓ reset {} settings — deleted {} (Ctrl+Z to restore)",
                    scope.label(),
                    path.display()
                )
            } else {
                // No file existed; nothing was actually removed, but
                // restating it as "already at defaults" reads cleaner
                // than a silent no-op.
                state.undo_stack.pop();
                format!(
                    "{} tier already at inherited / default values.",
                    scope.label()
                )
            };
            notifications.push(msg, NotifySeverity::Success);
        }
        Err(err) => {
            state.undo_stack.pop();
            notifications.push(
                format!("Reset of {} failed: {err}", path.display()),
                NotifySeverity::Error,
            );
        }
    }
}

/// Locate the `[model].model` `FieldMeta` in `CONFIG_SECTIONS`. Used by
/// the provider-swap path to read the just-reset model id and to bind
/// the secondary TOML write to the right `toml_path`.
fn model_field_meta() -> &'static FieldMeta {
    CONFIG_SECTIONS
        .iter()
        .flat_map(|s| s.fields.iter())
        .find(|f| f.toml_path == ["model", "model"])
        .expect("model field exists in CONFIG_SECTIONS")
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
    // If the chosen id belongs to a different provider in the registry
    // (only possible when the picker was in `all_providers` mode), swap
    // the provider field first. This keeps the on-disk pair consistent
    // and avoids the symmetric bug we just guarded for the Space-cycle
    // path — a registry lookup that fails (custom Ctrl+Enter id) simply
    // skips the provider swap, since we can't infer the provider.
    let provider_field = &CONFIG_SECTIONS[0].fields[0];
    let current_provider = match (provider_field.get)(&state.effective) {
        FieldValue::Enum(s) => s,
        _ => "openai",
    };
    let picked_provider = squeezy_llm::MODEL_REGISTRY
        .iter()
        .find(|m| m.id == model_id)
        .map(|m| m.provider);
    if let Some(new_provider) = picked_provider
        && new_provider != current_provider
        && let Err(msg) = (provider_field.set)(&mut state.effective, FieldValue::Enum(new_provider))
    {
        notifications.push(format!("invalid provider: {msg}"), NotifySeverity::Error);
        return;
    }
    // After a provider swap `set_provider` rewrote `cfg.model` to that
    // provider's default; overwrite it again with the user's chosen id
    // and save through the regular pipeline, which now persists
    // (provider, model) together.
    let model_field = model_field_meta();
    let model_value = FieldValue::String(model_id);
    if let Err(msg) = (model_field.set)(&mut state.effective, model_value.clone()) {
        notifications.push(format!("invalid: {msg}"), NotifySeverity::Error);
        return;
    }
    state.dirty = true;
    if let Some(new_provider) = picked_provider
        && new_provider != current_provider
    {
        // Persist the provider swap first (which chains the model write
        // via the provider→model pairing in `save_field_inner`), then
        // persist the explicit model id the user picked.
        save_field(
            state,
            agent,
            notifications,
            provider_field,
            FieldValue::Enum(new_provider),
        );
    }
    save_field(state, agent, notifications, model_field, model_value);
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
                reveal: false,
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
            entry.wipe();
            state.secret_entry = None;
        }
        (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
            entry.reveal = !entry.reveal;
        }
        (KeyCode::Backspace, _) => {
            entry.backspace();
        }
        (KeyCode::Left, _) => {
            entry.cursor = entry.cursor.saturating_sub(1);
        }
        (KeyCode::Right, _) => {
            entry.cursor = (entry.cursor + 1).min(entry.char_len());
        }
        (KeyCode::Home, _) => entry.cursor = 0,
        (KeyCode::End, _) => entry.cursor = entry.char_len(),
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            // Bracketed paste delivers each char individually through the
            // event loop, so this handles both interactive typing and
            // paste from the clipboard.
            entry.insert_char(c);
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
    save_field_inner(state, agent, notifications, field, value, false);
}

/// `silent=true` skips the per-save notification — used by Space-cycling
/// so the user doesn't see "saved …" pile up while flipping through
/// values. The actual file write and apply-tier dispatch still happen.
fn save_field_silent(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut NotificationQueue,
    field: &'static FieldMeta,
    value: FieldValue,
) {
    save_field_inner(state, agent, notifications, field, value, true);
}

fn save_field_inner(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut NotificationQueue,
    field: &'static FieldMeta,
    value: FieldValue,
    silent: bool,
) {
    let scope = state.scope;
    let target_path = match scope {
        ConfigScope::User => state.sources.user_path_default.clone(),
        ConfigScope::Repo => state.sources.project_path_default.clone(),
        ConfigScope::Local => state.sources.repo_path_default.clone(),
    };
    let scope_target = match scope {
        ConfigScope::User => SettingsScope::user(&target_path),
        ConfigScope::Repo => SettingsScope::project(&target_path),
        ConfigScope::Local => SettingsScope::repo(&target_path),
    };

    // Snapshot file bytes before the write so Ctrl+Z can revert this
    // single save. `None` means the file didn't exist before.
    let pre_write_bytes = std::fs::read(&target_path).ok();
    state
        .undo_stack
        .push((target_path.clone(), pre_write_bytes));

    let mut edits: Vec<SettingsEdit> = vec![field_edit(field, &value)];
    // When the user flips `[model].provider`, the in-memory setter has
    // already replaced `cfg.model` with that provider's default — persist
    // the same swap to the tier file so we never leave a half-written
    // pair like (provider=anthropic, model=gpt-5-codex) on disk. Without
    // this, the next process start would surface an inconsistent state.
    if field.toml_path == ["model", "provider"]
        && let FieldValue::String(model_id) = (model_field_meta().get)(&state.effective)
    {
        edits.push(SettingsEdit {
            path: model_field_meta().toml_path,
            op: EditOp::SetString(model_id),
        });
    }
    let outcome = match apply_edits(&scope_target, &edits) {
        Ok(o) => o,
        Err(err) => {
            // Roll back the bookkeeping for the failed write so Ctrl+Z
            // doesn't try to revert a write that never happened.
            state.undo_stack.pop();
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

    apply_by_tier(state, agent, notifications, field, &outcome, silent);
}

fn apply_by_tier(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut NotificationQueue,
    field: &'static FieldMeta,
    outcome: &WriteOutcome,
    silent: bool,
) {
    let path_str = outcome.path.display().to_string();
    match field.tier {
        ApplyTier::Immediate => {
            agent.replace_config(state.effective.clone());
            if !silent {
                notifications.push(
                    format!("✓ saved {} to {}", field.label, path_str),
                    NotifySeverity::Success,
                );
            }
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
                // `provider_from_config` resolves the API key, which on Linux
                // talks to D-Bus via the `keyring` crate. zbus refuses to do
                // blocking I/O inside a tokio runtime and panics. We run the
                // build on a plain OS thread so the keychain call is not
                // observed by tokio.
                let provider_cfg = state.effective.provider.clone();
                let handle =
                    std::thread::spawn(move || squeezy_llm::provider_from_config(&provider_cfg));
                match handle.join() {
                    Ok(Ok(p)) => Some(p),
                    Ok(Err(err)) => {
                        notifications.push(
                            format!(
                                "saved to {} but the new provider failed to build: {err}",
                                path_str
                            ),
                            NotifySeverity::Error,
                        );
                        None
                    }
                    Err(_) => {
                        notifications.push(
                            format!(
                                "saved to {} but the provider builder thread panicked",
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
            if !silent {
                notifications.push(
                    format!(
                        "{} changed — applies on next prompt. Saved to {}",
                        field.label, path_str
                    ),
                    NotifySeverity::Info,
                );
            }
        }
        ApplyTier::Restart => {
            if !silent {
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

/// Clear the focused field's value in whichever tier the active scope tab
/// points at. The User scope returns early in the caller, so this only runs
/// for Repo (committed `./squeezy.toml`) or Local (per-machine).
/// Ctrl+Z — restore the most recent edited tier file to its
/// pre-write bytes and refresh the agent + sources. No-op when the
/// session hasn't written anything yet.
fn undo_last_write(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut NotificationQueue,
) {
    let Some((path, pre_bytes)) = state.undo_stack.pop() else {
        notifications.push("Nothing to undo this session.", NotifySeverity::Info);
        return;
    };
    if let Err(err) = restore_path(&path, pre_bytes.as_deref()) {
        notifications.push(
            format!("Undo failed for {}: {err}", path.display()),
            NotifySeverity::Error,
        );
        // Put the snapshot back so a retry is possible.
        state.undo_stack.push((path, pre_bytes));
        return;
    }
    reload_sources_and_agent(state, agent, notifications);
    notifications.push(
        format!("✓ undid last write to {}", path.display()),
        NotifySeverity::Success,
    );
}

/// Discard every write made since the screen opened by restoring each
/// tier file to its baseline bytes. Clears the undo stack.
fn discard_all_session_writes(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut NotificationQueue,
) {
    if state.undo_stack.is_empty() {
        notifications.push(
            "Nothing to discard — no writes this session.",
            NotifySeverity::Info,
        );
        return;
    }
    let mut restored = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for (path, baseline_bytes) in &state.baseline {
        if let Err(err) = restore_path(path, baseline_bytes.as_deref()) {
            failed.push(format!("{}: {err}", path.display()));
        } else {
            restored += 1;
        }
    }
    state.undo_stack.clear();
    reload_sources_and_agent(state, agent, notifications);
    if failed.is_empty() {
        notifications.push(
            format!("✓ discarded all session writes ({restored} files restored)"),
            NotifySeverity::Success,
        );
    } else {
        notifications.push(
            format!(
                "partial restore — {} ok, failures: {}",
                restored,
                failed.join("; ")
            ),
            NotifySeverity::Warn,
        );
    }
}

/// Either write `bytes` to `path` (overwriting) or remove `path` when
/// the baseline is `None` (the file didn't exist when the screen opened).
fn restore_path(path: &std::path::Path, bytes: Option<&[u8]>) -> std::io::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    match bytes {
        Some(b) => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, b)
        }
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        },
    }
}

/// Reload separated sources and refresh the agent's effective config so
/// the post-undo/discard/reset state propagates without requiring a
/// restart.
///
/// If the provider *variant* changed (e.g. anthropic → openai because a
/// Local override was reset), the LLM client must be rebuilt — otherwise
/// the next turn runs an Anthropic client against an OpenAI config and
/// fails immediately. The rebuild happens on a plain OS thread (same
/// trick as `apply_by_tier::NextPrompt`) so the Linux keychain doesn't
/// panic from inside a tokio runtime, and is armed as a NextPrompt swap
/// so any in-flight turn keeps its current client.
fn reload_sources_and_agent(
    state: &mut ConfigScreenState,
    agent: &mut Agent,
    notifications: &mut NotificationQueue,
) {
    if let Ok(reloaded) = load_separated_settings_sources() {
        state.sources = reloaded;
    }
    let new_cfg = match AppConfig::from_env_and_settings() {
        Ok(cfg) => cfg,
        Err(err) => {
            notifications.push(
                format!("(note) couldn't rebuild effective config: {err}"),
                NotifySeverity::Warn,
            );
            return;
        }
    };
    let provider_changed = std::mem::discriminant(&state.effective.provider)
        != std::mem::discriminant(&new_cfg.provider);
    state.effective = new_cfg.clone();
    if !provider_changed {
        agent.replace_config(new_cfg);
        return;
    }
    let provider_cfg = new_cfg.provider.clone();
    let handle = std::thread::spawn(move || squeezy_llm::provider_from_config(&provider_cfg));
    match handle.join() {
        Ok(Ok(provider)) => {
            agent.arm_config_swap(PendingConfigSwap {
                config: new_cfg,
                provider: Some(provider),
                display_note: Some("provider switched (settings reset)".to_string()),
            });
        }
        Ok(Err(err)) => {
            agent.replace_config(new_cfg);
            notifications.push(
                format!("provider switched in config but the new client failed to build: {err}"),
                NotifySeverity::Error,
            );
        }
        Err(_) => {
            agent.replace_config(new_cfg);
            notifications.push(
                "provider switched but the builder thread panicked",
                NotifySeverity::Error,
            );
        }
    }
}

/// Silent variant — used by Space-cycle when wrapping past the last
/// option to "inherit". Suppresses the chatty "cleared … (now inherited)"
/// notification that would otherwise pile up.
fn clear_scope_override_silent(
    state: &mut ConfigScreenState,
    _notifications: &mut NotificationQueue,
) {
    let field = state.current_field();
    let (path, scope_target) = match state.scope {
        ConfigScope::Repo => {
            let p = state.sources.project_path_default.clone();
            (p.clone(), SettingsScope::project(&p))
        }
        ConfigScope::Local => {
            let p = state.sources.repo_path_default.clone();
            (p.clone(), SettingsScope::repo(&p))
        }
        ConfigScope::User => return,
    };
    let edit = SettingsEdit {
        path: field.toml_path,
        op: EditOp::Unset,
    };
    // Snapshot for undo before the write.
    let pre = std::fs::read(&path).ok();
    state.undo_stack.push((path.clone(), pre));
    match apply_edits(&scope_target, &[edit]) {
        Ok(_) => {
            if let Ok(reloaded) = load_separated_settings_sources() {
                state.sources = reloaded;
            }
        }
        Err(_) => {
            // Failed write — drop the unused snapshot so Ctrl+Z stays in sync.
            state.undo_stack.pop();
        }
    }
}

fn clear_scope_override(state: &mut ConfigScreenState, notifications: &mut NotificationQueue) {
    let field = state.current_field();
    let (path, scope_target) = match state.scope {
        ConfigScope::Repo => {
            let p = state.sources.project_path_default.clone();
            (p.clone(), SettingsScope::project(&p))
        }
        ConfigScope::Local => {
            let p = state.sources.repo_path_default.clone();
            (p.clone(), SettingsScope::repo(&p))
        }
        ConfigScope::User => return, // caller filters this out
    };
    let edit = SettingsEdit {
        path: field.toml_path,
        op: EditOp::Unset,
    };
    let scope_label = state.scope.label();
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
                format!("{} had no {} override to clear", field.label, scope_label),
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
    fn tab(label: &'static str, subtitle: &'static str, active: bool) -> Vec<Span<'static>> {
        let marker = if active { "▸ " } else { "  " };
        let label_style = if active {
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        vec![
            Span::styled(
                marker,
                Style::default().fg(if active { GOLD } else { QUIET }),
            ),
            Span::styled(label, label_style),
            Span::styled(format!(" {subtitle}"), Style::default().fg(QUIET)),
        ]
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        "  Config  ",
        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(" │ ", Style::default().fg(QUIET)));
    spans.extend(tab(
        "User",
        "~/.squeezy/settings.toml",
        state.scope == ConfigScope::User,
    ));
    spans.push(Span::styled(
        " ▸ ",
        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
    ));
    spans.extend(tab(
        "Repo",
        "./squeezy.toml (committed)",
        state.scope == ConfigScope::Repo,
    ));
    spans.push(Span::styled(
        " ▸ ",
        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
    ));
    spans.extend(tab(
        "Local",
        "~/.squeezy/projects/<this>/settings.toml",
        state.scope == ConfigScope::Local,
    ));
    spans.push(Span::styled(
        "    Local overrides Repo overrides User",
        Style::default().fg(QUIET),
    ));
    if state.dirty {
        spans.push(Span::styled(
            "  (changes applied)",
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
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(RESET_ACTIONS.len() * 3 + 4);
    lines.push(Line::from(vec![
        Span::styled(
            section.label,
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(section.description, Style::default().fg(QUIET)),
    ]));
    lines.push(Line::raw(""));
    for (idx, action) in RESET_ACTIONS.iter().enumerate() {
        let active = idx == state.field_index;
        let prefix = if active { "› " } else { "  " };
        let prefix_style = Style::default().fg(if active { GOLD } else { QUIET });
        let label_style = if active {
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let tier_path = tier_path(state, action.scope);
        let exists = std::fs::metadata(&tier_path).is_ok();
        let status = if exists {
            Span::styled("[file present]", Style::default().fg(SUCCESS_GREEN))
        } else {
            Span::styled("[no file]", Style::default().fg(QUIET))
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, prefix_style),
            Span::styled(format!("{:<28}", action.label), label_style),
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
    }
    lines.push(Line::from(vec![
        Span::styled("? ", Style::default().fg(QUIET)),
        Span::styled(
            "Enter on a row to delete that tier's file (with y/n confirmation). Ctrl+Z restores it.",
            Style::default().fg(QUIET),
        ),
    ]));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_reset_confirm(frame: &mut Frame<'_>, area: Rect, state: &ConfigScreenState) {
    let scope = state.reset_confirm.expect("guarded by caller");
    let path = tier_path(state, scope);
    let exists = std::fs::metadata(&path).is_ok();
    let lines = vec![
        Line::from(vec![Span::styled(
            "Reset confirmation",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        )]),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  Delete the "),
            Span::styled(
                scope.label(),
                Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" settings file?"),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("    path  ", Style::default().fg(QUIET)),
            Span::raw(path.display().to_string()),
        ]),
        Line::from(vec![
            Span::styled("    status ", Style::default().fg(QUIET)),
            Span::styled(
                if exists { "exists" } else { "(no file)" },
                Style::default().fg(if exists { SUCCESS_GREEN } else { QUIET }),
            ),
        ]),
        Line::raw(""),
        Line::from(vec![Span::styled(
            "  Other tabs are not touched. Inherited / default values then take over,\n  \
             which may change values shown elsewhere — that's the point of a reset.",
            Style::default().fg(QUIET),
        )]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("y", Style::default().fg(GOLD).add_modifier(Modifier::BOLD)),
            Span::styled(" delete   ", Style::default().fg(QUIET)),
            Span::styled("n", Style::default().fg(GOLD).add_modifier(Modifier::BOLD)),
            Span::styled(" cancel   ", Style::default().fg(QUIET)),
            Span::styled(
                "Esc",
                Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" cancel", Style::default().fg(QUIET)),
        ]),
    ];
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
                lines.push(Line::from(vec![
                    Span::styled(prefix, prefix_style),
                    Span::styled(
                        format!("{:<width$}", field.label, width = max_label + 2),
                        label_style,
                    ),
                    Span::styled(
                        value_str,
                        Style::default().fg(if active { GOLD } else { Color::White }),
                    ),
                    Span::raw(" "),
                    Span::styled(source_label, source_style(source)),
                ]));
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

fn empty_sources_for(_cfg: &AppConfig) -> SeparatedSources {
    SeparatedSources {
        user: None,
        project: None,
        repo: None,
        user_path_default: PathBuf::from(""),
        project_path_default: PathBuf::from(""),
        repo_path_default: PathBuf::from(""),
    }
}

#[cfg(test)]
#[path = "config_screen_tests.rs"]
mod tests;
