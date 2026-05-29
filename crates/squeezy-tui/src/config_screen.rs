//! Full-page options UI invoked via `/options` or F11.
//!
//! Layout: two scope tabs (User / Project) on top, a section sidebar on the
//! left, a field editor on the right, and a footer hint row at the bottom.
//! Saves write the corresponding TOML file via `squeezy_core::settings_writer`
//! and apply changes by tier: `Immediate` → `agent.replace_config(...)`;
//! `NextPrompt` → `agent.arm_config_swap(...)`; `Restart` → notification only.

use std::path::PathBuf;

#[cfg(test)]
use crossterm::event::KeyModifiers;
use crossterm::event::{KeyCode, KeyEvent};
#[cfg(test)]
use squeezy_agent::Agent;
use squeezy_core::{
    AppConfig, SeparatedSources,
    config_schema::{
        CONFIG_SECTIONS, ConfigSectionMeta, FieldKind, FieldMeta, FieldSource, FieldValue,
        SectionId,
    },
    load_separated_settings_sources,
};

#[cfg(test)]
use crate::notification::NotificationQueue;

mod keys;
mod render;
mod save;

pub(crate) use keys::{handle_key, handle_paste};
pub(crate) use render::render;
pub(crate) use save::{
    clear_scope_override, clear_scope_override_silent, discard_all_session_writes, perform_reset,
    save_field, save_field_silent, save_inline_provider_api_key, undo_last_write,
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
pub(crate) const RESET_ACTIONS: &[ResetAction] = &[
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
    pub(crate) scope: ConfigScope,
    pub(crate) label: &'static str,
    pub(crate) detail: &'static str,
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
    pub(crate) fn char_len(&self) -> usize {
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
    pub(crate) fn insert_char(&mut self, c: char) {
        if let Some(byte_idx) = self.char_to_byte(self.cursor) {
            self.draft.insert(byte_idx, c);
            self.cursor += 1;
        }
    }

    /// Delete the char immediately to the left of the cursor (backspace).
    /// No-op when the cursor is already at the start.
    pub(crate) fn backspace(&mut self) {
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
    pub(crate) fn wipe(&mut self) {
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
            scope: ConfigScope::Local,
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
            // The Reset section only ever surfaces the action for the active
            // scope tab — resetting another tab's file from here would be
            // confusing and the tier-tab context already disambiguates which
            // file is being targeted.
            SectionId::Reset => 1,
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
    /// There is exactly one row, and it always targets the active scope
    /// tab's file.
    pub(crate) fn reset_action_at_row(&self, _row: usize) -> Option<&'static ResetAction> {
        if self.current_section().id == SectionId::Reset {
            RESET_ACTIONS.iter().find(|a| a.scope == self.scope)
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

    /// Walk the full precedence chain (Local → Repo → User → Default),
    /// returning the value and the tier that supplies it. Independent
    /// of `self.scope` — this is the *running* effective value, not the
    /// per-tab view. Env-shadowed fields short-circuit to the env value
    /// and `FieldSource::Env`.
    fn effective_value_full(&self, field: &FieldMeta) -> (FieldValue, FieldSource) {
        self.effective_value_skipping(field, None)
    }

    /// Same as `effective_value_full`, but pretend the file behind
    /// `skip` (if any) doesn't exist. Used by the Reset preview to
    /// answer "what would this field display if I deleted that tier
    /// file right now?"
    fn effective_value_skipping(
        &self,
        field: &FieldMeta,
        skip: Option<ConfigScope>,
    ) -> (FieldValue, FieldSource) {
        if let Some(var) = field.env_override
            && std::env::var(var).is_ok()
        {
            return ((field.get)(&self.effective), FieldSource::Env);
        }
        let chain: &[(FieldSource, ConfigScope, &Option<squeezy_core::TierSource>)] = &[
            (FieldSource::Repo, ConfigScope::Local, &self.sources.repo),
            (
                FieldSource::Project,
                ConfigScope::Repo,
                &self.sources.project,
            ),
            (FieldSource::User, ConfigScope::User, &self.sources.user),
        ];
        for (src, owns_scope, tier) in chain {
            if Some(*owns_scope) == skip {
                continue;
            }
            if let Some(t) = tier
                && let Some(val) = tier_value_at_path(t, field)
            {
                return (val, *src);
            }
        }
        ((field.default)(), FieldSource::Default)
    }

    /// Compute the list of fields whose effective value would change if
    /// `scope`'s tier file were deleted right now. Used by the Reset
    /// confirmation overlay so the user sees exactly which configured
    /// values they're about to lose.
    ///
    /// Env-shadowed fields are skipped — reset can't move them.
    /// Schema kinds we don't yet render in the screen
    /// (`TableArray`, `ProviderSubTabs`) are also skipped because
    /// `tier_value_at_path` returns `None` for them and the diff would
    /// be lossy.
    pub(crate) fn reset_preview(&self, scope: ConfigScope) -> Vec<ResetPreviewEntry> {
        let mut out: Vec<ResetPreviewEntry> = Vec::new();
        for section in CONFIG_SECTIONS {
            if section.id == SectionId::Reset {
                continue;
            }
            for field in section.fields {
                if matches!(
                    field.kind,
                    FieldKind::TableArray { .. }
                        | FieldKind::ProviderSubTabs
                        | FieldKind::Secret { .. }
                ) {
                    continue;
                }
                let (before, before_src) = self.effective_value_full(field);
                if before_src == FieldSource::Env {
                    continue;
                }
                let (after, after_src) = self.effective_value_skipping(field, Some(scope));
                if before != after {
                    out.push(ResetPreviewEntry {
                        section_label: section.label,
                        field_label: field.label,
                        before: before.as_display(),
                        after: after.as_display(),
                        after_source: after_src,
                    });
                }
            }
        }
        out
    }
}

/// One row of the Reset preview shown inside the y/n confirmation.
/// `after_source` lets the renderer attach the same `[inherited-…]`
/// vocabulary used everywhere else.
#[derive(Debug, Clone)]
pub(crate) struct ResetPreviewEntry {
    pub section_label: &'static str,
    pub field_label: &'static str,
    pub before: String,
    pub after: String,
    pub after_source: FieldSource,
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
/// Returns `[env]` when the running value is dictated by an environment
/// variable — the only case worth surfacing inline, because env-shadowed
/// fields are inert in the editor and Enter / Space refuses to write
/// them. Every other source (own tier, inherited tier, binary default)
/// is rendered without a trailing badge: the displayed value is the
/// effective one, the tier the user is editing is already visible in
/// the tab strip, and badges like "repo" or "[inherited-default]" turn
/// out to be noise that the user has to mentally filter on every row.
pub(crate) fn inheritance_label(_active: ConfigScope, source: FieldSource) -> String {
    if source == FieldSource::Env {
        "[env]".to_string()
    } else {
        String::new()
    }
}

/// Outcome of a single key press on the screen. `Close` tells the host to
/// hide the screen; `KeepOpen` keeps it; `OpenedExternal` is reserved for
/// future "open shell" actions.
pub(crate) enum KeyOutcome {
    KeepOpen,
    Close,
}

#[derive(Debug)]
pub(crate) enum EditorOutcome {
    KeepEditing,
    Commit(FieldValue),
    Cancel,
}

pub(crate) fn open_editor_for(field: &FieldMeta, current: FieldValue) -> FieldEditor {
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

pub(crate) fn handle_editor_key(editor: &mut FieldEditor, key: KeyEvent) -> EditorOutcome {
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
pub(crate) fn tier_path(state: &ConfigScreenState, scope: ConfigScope) -> std::path::PathBuf {
    match scope {
        ConfigScope::User => state.sources.user_path_default.clone(),
        ConfigScope::Repo => state.sources.project_path_default.clone(),
        ConfigScope::Local => state.sources.repo_path_default.clone(),
    }
}

/// Human-readable name for a `ProviderConfig` variant. Used in the
/// PendingConfigSwap display note so reset / provider-swap toasts read
/// "openai → anthropic" instead of an opaque "provider switched".
pub(crate) fn provider_variant_label(provider: &squeezy_core::ProviderConfig) -> &'static str {
    use squeezy_core::ProviderConfig as P;
    match provider {
        P::OpenAi(_) => "openai",
        P::Anthropic(_) => "anthropic",
        P::Google(_) => "google",
        P::AzureOpenAi(_) => "azure_openai",
        P::Bedrock(_) => "bedrock",
        P::Ollama(_) => "ollama",
        P::OpenAiCodex(_) => "openai_codex",
        P::OpenAiCompatible(config) => config.preset.as_str(),
        P::Faux(_) => "faux",
    }
}

/// Locate the `[model].model` `FieldMeta` in `CONFIG_SECTIONS`. Used by
/// the provider-swap path to read the just-reset model id and to bind
/// the secondary TOML write to the right `toml_path`.
pub(crate) fn model_field_meta() -> &'static FieldMeta {
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

pub(crate) fn picker_matches(state: &ModelPickerState) -> Vec<&'static squeezy_llm::ModelInfo> {
    let filter_lower = state.filter.to_lowercase();
    squeezy_llm::MODEL_REGISTRY
        .iter()
        .filter(|m| state.all_providers || m.provider == state.current_provider)
        .filter(|m| filter_lower.is_empty() || m.id.to_lowercase().contains(&filter_lower))
        .collect()
}

// ─── API key (Secret) entry ───────────────────────────────────────────────────

pub(crate) fn provider_api_key_env(
    provider: &squeezy_core::ProviderConfig,
) -> Option<(&'static str, String)> {
    use squeezy_core::ProviderConfig as P;
    match provider {
        P::OpenAi(c) => Some(("OpenAI", c.api_key_env.clone())),
        P::Anthropic(c) => Some(("Anthropic", c.api_key_env.clone())),
        P::Google(c) => Some(("Google", c.api_key_env.clone())),
        P::AzureOpenAi(c) => Some(("Azure OpenAI", c.api_key_env.clone())),
        // Bedrock uses AWS SDK creds; Ollama is local; the ChatGPT
        // Codex provider stores OAuth tokens at `~/.squeezy/auth/`;
        // the faux provider runs in-process and has no credential —
        // none of these have an env-var keychain entry the screen
        // can write.
        P::Bedrock(_) | P::Ollama(_) | P::OpenAiCodex(_) | P::Faux(_) => None,
        P::OpenAiCompatible(c) => {
            if c.api_key_env.is_empty() {
                None
            } else {
                Some((c.preset.display_name(), c.api_key_env.clone()))
            }
        }
    }
}

/// Return the `[providers.<section>]` table name for the current provider,
/// so the TUI Enter handler can write the inline `api_key` to the right
/// part of the TOML.
pub(crate) fn provider_section_name(
    provider: &squeezy_core::ProviderConfig,
) -> Option<&'static str> {
    use squeezy_core::ProviderConfig as P;
    match provider {
        P::OpenAi(_) => Some("openai"),
        P::Anthropic(_) => Some("anthropic"),
        P::Google(_) => Some("google"),
        P::AzureOpenAi(_) => Some("azure_openai"),
        // The Codex provider's credentials live in the OAuth token
        // file, not the providers TOML table, so there's no section
        // to write. The faux provider exposes `script` instead of an
        // api_key, which is handled by the field-level editor rather
        // than the secret-entry path.
        P::Bedrock(_) | P::Ollama(_) | P::OpenAiCodex(_) | P::Faux(_) => None,
        P::OpenAiCompatible(c) => Some(c.preset.as_str()),
    }
}

/// Read the currently-stored inline `api_key` for the active provider out of
/// the merged config TOML (user + repo + local), without touching env vars or
/// secrets stores. Used by the secret-entry pre-fill so reopening the field
/// shows the value the user previously saved.
pub(crate) fn provider_inline_api_key(provider: &squeezy_core::ProviderConfig) -> Option<String> {
    use squeezy_core::ProviderConfig as P;
    match provider {
        P::OpenAi(c) => c.api_key.clone(),
        P::Anthropic(c) => c.api_key.clone(),
        P::Google(c) => c.api_key.clone(),
        P::AzureOpenAi(c) => c.api_key.clone(),
        P::Bedrock(_) | P::Ollama(_) | P::OpenAiCodex(_) | P::Faux(_) => None,
        P::OpenAiCompatible(c) => c.api_key.clone(),
    }
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
