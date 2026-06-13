//! Full-page config UI invoked via `/config` or F11.
//!
//! Layout: three scope tabs (User / Repo / Local) on top, a section sidebar on
//! the left, a field editor on the right, and a footer hint row at the bottom.
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
    AppConfig, PermissionPolicyMode, SeparatedSources,
    config_schema::{
        ApplyTier, CONFIG_SECTIONS, ConfigSectionMeta, FieldKind, FieldMeta, FieldSource,
        FieldValue, SectionId,
    },
    load_separated_settings_sources,
};

mod keys;
mod render;
mod save;

pub(crate) use keys::{handle_key, handle_paste};
pub(crate) use render::render;
pub(crate) use save::{
    clear_scope_override, clear_scope_override_silent, discard_all_session_writes, perform_reset,
    permission_detail_read_path, save_field, save_field_silent, save_inline_provider_api_key,
    save_theme_color, save_theme_delete, save_theme_rename, save_theme_selection,
    save_theme_snapshot, undo_last_write, unset_theme_color,
};

/// Severity tag for a single feedback line emitted by the config screen.
/// The host maps it onto the transcript: `Error` / `Warn` render a `⚠`
/// warning line, `Info` / `Success` render dim operational chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Info,
    Success,
    Warn,
    Error,
}

/// One accumulated feedback line, carrying the message and its severity so
/// the host can route it to the right transcript surface.
#[derive(Debug, Clone)]
pub(crate) struct FeedbackEntry {
    pub message: String,
    pub severity: Severity,
}

/// Feedback sink threaded through the config screen's key/save handlers.
///
/// The screen used to push fire-and-forget lines into a rotating
/// notification pane; that pane is gone. Handlers now accumulate their
/// feedback here and the host (`lib.rs`) drains it into the durable
/// transcript after each key press.
#[derive(Debug, Default)]
pub(crate) struct ConfigFeedback {
    entries: Vec<FeedbackEntry>,
}

impl ConfigFeedback {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record a feedback line. Mirrors the old queue's `push` signature so
    /// the handler bodies stay unchanged.
    pub(crate) fn push(&mut self, message: impl Into<String>, severity: Severity) {
        self.entries.push(FeedbackEntry {
            message: message.into(),
            severity,
        });
    }

    /// Drain every accumulated line in emission order, leaving the sink
    /// empty. The host forwards each to the transcript.
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = FeedbackEntry> + '_ {
        self.entries.drain(..)
    }

    /// The most recently pushed line, if any. Used by tests to assert on
    /// the feedback a handler emitted.
    #[cfg(test)]
    pub(crate) fn current(&self) -> Option<&FeedbackEntry> {
        self.entries.last()
    }
}

/// Synthetic row index in the Models section that exposes the API-key
/// editor for the currently selected provider. Sits right after `model`
/// so provider + model + key read top-to-bottom as a single "what model
/// am I talking to and with which credential" cluster. Not backed by a
/// `FieldMeta` in `CONFIG_SECTIONS`.
const SYNTHETIC_KEY_ROW: usize = 2;

/// Number of Auto-review reviewer rows (`reviewer_model`, `reviewer_policy`,
/// `reviewer_policy_extra`, `reviewer_capabilities`) that follow `mode` in the
/// Permissions section's field list.
const PERMISSION_REVIEWER_ROWS: usize = 4;

/// Visible row count for the Permissions section. Rows are a contiguous
/// prefix of the section's field list — `mode` only for Default/Full Access,
/// plus the reviewer rows for Auto-review, plus every per-capability row for
/// Custom. Keeping it a prefix means `field_at_row(row) == fields[row]`.
fn permissions_visible_rows(mode: PermissionPolicyMode, field_count: usize) -> usize {
    match mode {
        PermissionPolicyMode::Custom => field_count,
        PermissionPolicyMode::AutoReview => 1 + PERMISSION_REVIEWER_ROWS,
        PermissionPolicyMode::Default | PermissionPolicyMode::FullAccess => 1,
    }
}

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

/// Inputs and result for [`ConfigScreenState::credential_source`]'s memo:
/// the provider env var and inline key the source was resolved for, so a
/// later render can reuse it without re-walking the credential chain.
struct CredentialSourceCache {
    env_var: String,
    inline: Option<String>,
    source: Option<squeezy_llm::KeySource>,
}

pub(crate) struct ConfigScreenState {
    pub scope: ConfigScope,
    pub section_index: usize,
    pub field_index: usize,
    pub editor: Option<FieldEditor>,
    /// Full-screen multi-line editor for long String fields (the routing judge
    /// prompt). The inline single-line caret can't show or edit a paragraph.
    pub prompt_editor: Option<PromptEditorState>,
    pub picker: Option<ModelPickerState>,
    pub search: Option<SearchOverlayState>,
    pub secret_entry: Option<SecretEntryState>,
    pub theme_editor: Option<ThemeEditor>,
    /// Pending tier-file deletion awaiting `y/n` confirmation. Set when the
    /// user presses Enter on a Reset-section row; cleared by `y` (after the
    /// delete fires) or `n` / Esc (cancel).
    pub reset_confirm: Option<ConfigScope>,
    /// Pending "discard everything written this session" awaiting `y/n`
    /// confirmation. Set when the user presses Shift+X in browse mode;
    /// cleared by `y` (after the discard fires) or `n` / Esc (cancel).
    /// Without a confirmation gate, a stray capital X would silently
    /// undo every save made since the screen opened.
    pub discard_confirm: bool,
    pub effective: AppConfig,
    /// Memoized credential source for the synthetic API-key row. The render
    /// path reads this instead of re-resolving every frame (resolution
    /// stats/reads credentials.json), recomputed only when the active
    /// provider's env var or inline key changes. `RefCell` mirrors the
    /// crate's other render-time caches.
    credential_source_cache: std::cell::RefCell<Option<CredentialSourceCache>>,
    pub sources: SeparatedSources,
    pub dirty: bool,
    /// File bytes captured the moment the screen opened, per tier path.
    /// `Discard all` rewrites every file to its baseline.
    pub baseline: Vec<(std::path::PathBuf, Option<Vec<u8>>)>,
    /// `(path, pre_write_bytes)` for every write since the screen opened.
    /// `Ctrl+Z` pops the last entry and rewrites the file to its
    /// pre-write contents.
    pub undo_stack: Vec<(std::path::PathBuf, Option<Vec<u8>>)>,
    pub telemetry_undo_markers: Vec<usize>,
    pub telemetry_changes: Vec<ConfigTelemetryChange>,
    /// Snapshot of the live MCP server map. Mirrors what
    /// `Agent::mcp_servers()` returns. Refreshed by the host after
    /// every key + on every draw so the table reflects in-flight
    /// toggles/restarts.
    pub mcp_servers: std::collections::BTreeMap<String, squeezy_core::McpServerConfig>,
    /// Live MCP status snapshot — ready/failed/cancelled/starting,
    /// per-server tool counts. Owned by the agent; the host refreshes
    /// this each draw.
    pub mcp_status: squeezy_tools::McpStatusSnapshot,
    /// Pending name awaiting `y/n` confirmation to remove from the
    /// MCP page. Tracks the active scope at confirm-time so the
    /// follow-up TOML write hits the right file.
    pub mcp_pending_delete: Option<String>,
    /// Add-server overlay state. Present when the user has pressed
    /// `a` on the McpServers section; absent during normal browse.
    pub mcp_add: Option<McpAddForm>,
    /// Actions staged by sync key handlers; the host's async loop
    /// drains them and routes to `agent.set_mcp_server_enabled` /
    /// `restart_mcp_server` / `replace_mcp_servers` before the next
    /// draw.
    pub mcp_pending_actions: Vec<McpAction>,
    /// Last successful MCP-action feedback line, e.g. `enabled bench
    /// (session-only)`. Rendered above the server list so a user
    /// pressing `e`/`r` sees confirmation even before discovery
    /// completes.
    pub mcp_last_status_line: Option<String>,
    /// Mirror of `TuiApp::animation_tick`, copied each frame by the
    /// host so the status-indicator glyph for a server that is
    /// currently discovering / restarting can pulse through a
    /// short cycle (`◐ ◓ ◑ ◒`) instead of standing still. The
    /// integer wraps; the render path only ever does `% N`.
    pub mcp_animation_tick: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigTelemetryChange {
    pub scope: ConfigScope,
    pub section: &'static str,
    pub field: String,
    pub apply_tier: ApplyTier,
    pub change_kind: ConfigTelemetryChangeKind,
    pub prev_value: String,
    pub new_value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigTelemetryChangeKind {
    Set,
    Unset,
    Reset,
}

/// Full-screen multi-line text editor for long String fields (e.g. the routing
/// judge prompt). Models the buffer exactly like the main composer in
/// `input.rs`: a flat `String` with a byte `cursor`, so newlines live in the
/// text and the renderer soft-wraps to the pane width. Vertical scroll is
/// derived from the cursor at render time, so it isn't stored here.
pub(crate) struct PromptEditorState {
    pub draft: String,
    /// Byte index into `draft`. Always kept on a char boundary.
    pub cursor: usize,
}

impl PromptEditorState {
    pub(crate) fn new(draft: String) -> Self {
        let cursor = draft.len();
        Self { draft, cursor }
    }

    /// Snap the cursor to the nearest char boundary at or before its position.
    fn clamp(&mut self) {
        let mut c = self.cursor.min(self.draft.len());
        while c > 0 && !self.draft.is_char_boundary(c) {
            c -= 1;
        }
        self.cursor = c;
    }

    pub(crate) fn insert_char(&mut self, ch: char) {
        self.clamp();
        self.draft.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    pub(crate) fn backspace(&mut self) {
        self.clamp();
        if self.cursor == 0 {
            return;
        }
        let prev = self.draft[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.draft.drain(prev..self.cursor);
        self.cursor = prev;
    }

    pub(crate) fn delete(&mut self) {
        self.clamp();
        if self.cursor >= self.draft.len() {
            return;
        }
        let next = self.cursor
            + self.draft[self.cursor..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(0);
        self.draft.drain(self.cursor..next);
    }

    pub(crate) fn left(&mut self) {
        self.clamp();
        self.cursor = self.draft[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }

    pub(crate) fn right(&mut self) {
        self.clamp();
        if self.cursor >= self.draft.len() {
            return;
        }
        self.cursor += self.draft[self.cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0);
    }

    fn line_start(&self) -> usize {
        self.draft[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    fn line_end(&self) -> usize {
        self.draft[self.cursor..]
            .find('\n')
            .map(|o| self.cursor + o)
            .unwrap_or(self.draft.len())
    }

    pub(crate) fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub(crate) fn end(&mut self) {
        self.cursor = self.line_end();
    }

    /// Move up one logical line, preserving the byte column. Mirrors
    /// `input.rs::move_input_cursor_up`.
    pub(crate) fn up(&mut self) {
        self.clamp();
        let curr_start = self.line_start();
        if curr_start == 0 {
            self.cursor = 0;
            return;
        }
        let col = self.cursor - curr_start;
        let prev_end = curr_start - 1;
        let prev_start = self.draft[..prev_end]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let prev_len = prev_end - prev_start;
        self.cursor = prev_start + col.min(prev_len);
        self.clamp();
    }

    /// Move down one logical line, preserving the byte column.
    pub(crate) fn down(&mut self) {
        self.clamp();
        let curr_start = self.line_start();
        let col = self.cursor - curr_start;
        let Some(next_start) = self.draft[curr_start..]
            .find('\n')
            .map(|o| curr_start + o + 1)
        else {
            self.cursor = self.draft.len();
            return;
        };
        let next_end = self.draft[next_start..]
            .find('\n')
            .map(|o| next_start + o)
            .unwrap_or(self.draft.len());
        let next_len = next_end - next_start;
        self.cursor = next_start + col.min(next_len);
        self.clamp();
    }
}

/// How much of a secret the entry overlay discloses. F2 cycles
/// `Hidden → LastFour → Full → Hidden`, so the user can verify a pasted
/// key by its suffix (the common "did the right key land?" check) without
/// exposing the whole secret on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SecretReveal {
    /// Every character masked (`••••`).
    #[default]
    Hidden,
    /// Mask all but the last four characters (`••••…abc123`).
    LastFour,
    /// Full plaintext, the user's own deliberate disclosure.
    Full,
}

impl SecretReveal {
    /// F2 cycle order: `Hidden → LastFour → Full → Hidden`.
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Hidden => Self::LastFour,
            Self::LastFour => Self::Full,
            Self::Full => Self::Hidden,
        }
    }
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
    /// How much of the key the render discloses. F2 cycles through the three
    /// states; the user starts fully masked and opts into more on purpose.
    pub reveal: SecretReveal,
}

#[derive(Debug, Clone)]
pub(crate) enum ThemeEditor {
    Name {
        draft: String,
        cursor: usize,
    },
    Rename {
        original: String,
        draft: String,
        cursor: usize,
    },
    Rgb {
        theme: String,
        token: &'static str,
        draft: String,
        cursor: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ThemeRow {
    Theme(String),
    New,
    Color(&'static str),
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

/// Live filter over `CONFIG_SECTIONS`. Opened by typing any printable
/// character (or `/`) in browse mode; the body collapses to a small box on
/// top plus the reduced match list. `Enter` jumps to the matched setting,
/// keeping the active scope tab. Matches section/field names and each
/// field's help text.
pub(crate) struct SearchOverlayState {
    pub query: String,
    /// Index into `matches` of the highlighted result.
    pub cursor: usize,
    /// Matches sorted best-first (highest score; see [`compute_search_matches`]).
    pub matches: Vec<SearchMatch>,
}

/// What a [`SearchMatch`] points at within its section. Most rows are real
/// schema fields, but the Models section also exposes a synthetic "API key" row
/// with no `FieldMeta`, and the field-less sections (Themes / McpServers /
/// Reset) match at section level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchTarget {
    /// A real schema field, by its raw index into `section.fields`.
    Field(usize),
    /// The synthetic API-key row in the Models section (no `FieldMeta`).
    SyntheticApiKey,
    /// A field-less section — focus its first row.
    Section,
}

/// One filter result: a section plus what to focus inside it, with its score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SearchMatch {
    pub section_index: usize,
    pub target: SearchTarget,
    pub score: i32,
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
    OptionalFloat {
        draft: String,
        cursor: usize,
        min: f64,
        max: f64,
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
        let mcp_servers = effective.mcp_servers.clone();
        Self {
            // Default to the User tab — it's the most relevant tier for
            // first-time users (and the only one writable when there is
            // no repo / project file yet). The tab strip renders
            // User → Repo → Local left-to-right, so opening on the
            // leftmost tab also matches reading order.
            scope: ConfigScope::User,
            section_index,
            field_index: 0,
            editor: None,
            prompt_editor: None,
            picker: None,
            search: None,
            secret_entry: None,
            theme_editor: None,
            reset_confirm: None,
            discard_confirm: false,
            effective,
            credential_source_cache: std::cell::RefCell::new(None),
            sources,
            dirty: false,
            baseline,
            undo_stack: Vec::new(),
            telemetry_undo_markers: Vec::new(),
            telemetry_changes: Vec::new(),
            mcp_servers,
            mcp_status: Default::default(),
            mcp_pending_delete: None,
            mcp_add: None,
            mcp_pending_actions: Vec::new(),
            mcp_last_status_line: None,
            mcp_animation_tick: 0,
        }
    }

    /// Resolved credential source for the synthetic API-key row, memoized so
    /// the render path performs no per-frame credential resolution (which
    /// stats/reads credentials.json). The full runtime chain
    /// (`resolve_api_key_with_inline`) runs only when the provider's env var
    /// or inline key changes — provider swap or inline-key save — so a config
    /// screen left open while a turn animates does not re-read the credentials
    /// file every frame. Returns `None` for providers without an env-var
    /// credential or when nothing resolves. Only the source is retained; the
    /// secret value is never kept.
    pub(crate) fn credential_source(
        &self,
        env_var: &str,
        inline: Option<&str>,
    ) -> Option<squeezy_llm::KeySource> {
        if env_var.is_empty() {
            return None;
        }
        let mut cache = self.credential_source_cache.borrow_mut();
        let fresh = cache
            .as_ref()
            .is_some_and(|c| c.env_var == env_var && c.inline.as_deref() == inline);
        if !fresh {
            let source = squeezy_llm::resolve_api_key_with_inline(inline, env_var)
                .ok()
                .map(|resolved| resolved.source);
            *cache = Some(CredentialSourceCache {
                env_var: env_var.to_string(),
                inline: inline.map(str::to_string),
                source,
            });
        }
        cache.as_ref().and_then(|c| c.source)
    }

    /// Sorted list of configured server names — render path uses this
    /// to map a row index to a server entry on the `/mcp` page.
    pub(crate) fn mcp_server_names(&self) -> Vec<String> {
        self.mcp_servers.keys().cloned().collect()
    }

    /// Server entry at row `row` on the `/mcp` page. Returns `None` if
    /// the row is out of bounds (the "add new" row sits at
    /// `mcp_server_names().len()` and has no server entry yet).
    pub(crate) fn mcp_server_at_row(
        &self,
        row: usize,
    ) -> Option<(String, &squeezy_core::McpServerConfig)> {
        let names = self.mcp_server_names();
        names
            .get(row)
            .and_then(|name| self.mcp_servers.get(name).map(|cfg| (name.clone(), cfg)))
    }
}

impl ConfigScreenState {
    pub(crate) fn push_undo_snapshot(
        &mut self,
        path: std::path::PathBuf,
        pre_write_bytes: Option<Vec<u8>>,
    ) {
        self.undo_stack.push((path, pre_write_bytes));
        self.telemetry_undo_markers
            .push(self.telemetry_changes.len());
    }

    pub(crate) fn pop_undo_snapshot(
        &mut self,
    ) -> Option<(std::path::PathBuf, Option<Vec<u8>>, usize)> {
        let marker = self
            .telemetry_undo_markers
            .pop()
            .unwrap_or(self.telemetry_changes.len());
        self.undo_stack
            .pop()
            .map(|(path, pre_write_bytes)| (path, pre_write_bytes, marker))
    }

    pub(crate) fn truncate_telemetry_to(&mut self, marker: usize) {
        self.telemetry_changes
            .truncate(marker.min(self.telemetry_changes.len()));
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
    /// Permission mode as shown in the `mode` row (resolved against the active
    /// scope's saved sources). Visibility of the reviewer rows tracks this so
    /// they stay consistent with the value the user actually sees, even when the
    /// agent snapshot behind `effective` lags a freshly-saved settings file.
    fn displayed_permission_mode(&self) -> Option<PermissionPolicyMode> {
        let section = CONFIG_SECTIONS
            .iter()
            .find(|s| s.id == SectionId::Permissions)?;
        let mode_field = section
            .fields
            .iter()
            .find(|f| f.toml_path == ["permissions", "mode"])?;
        match self.displayed_value_and_source(mode_field).0 {
            FieldValue::Enum(s) => PermissionPolicyMode::parse(s),
            _ => None,
        }
    }

    /// Visible Permissions rows: the larger of what the running config
    /// (`effective`) and the displayed (saved) mode would show, so the reviewer
    /// rows appear whenever either indicates Auto-review/Custom. This keeps the
    /// rows from disappearing when the agent snapshot and the saved file
    /// disagree about the mode at open time.
    fn permission_visible_rows(&self, field_count: usize) -> usize {
        let by_effective = permissions_visible_rows(self.effective.permissions.mode, field_count);
        match self.displayed_permission_mode() {
            Some(mode) => by_effective.max(permissions_visible_rows(mode, field_count)),
            None => by_effective,
        }
    }

    /// synthetic "API key" row for the Models section and the three
    /// per-tier action rows for the Reset section.
    pub(crate) fn row_count(&self) -> usize {
        let section = self.current_section();
        match section.id {
            SectionId::Models => section.fields.len() + 1,
            SectionId::Permissions => self.permission_visible_rows(section.fields.len()),
            // The Reset section only ever surfaces the action for the active
            // scope tab — resetting another tab's file from here would be
            // confusing and the tier-tab context already disambiguates which
            // file is being targeted.
            SectionId::Reset => 1,
            SectionId::Themes => {
                crate::render::theme::available_theme_names(&self.effective).len()
                    + 1
                    + crate::render::theme::token_rows().len()
            }
            // Server rows + one trailing "(add)" row so the user can
            // bring up the add overlay from the same focus loop.
            SectionId::McpServers => self.mcp_servers.len() + 1,
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
            SectionId::Permissions => {
                let visible = self.permission_visible_rows(section.fields.len());
                (row < visible).then(|| section.fields.get(row)).flatten()
            }
            SectionId::Reset | SectionId::Themes | SectionId::McpServers => None,
            _ => section.fields.get(row),
        }
    }

    /// Inverse of [`field_at_row`]: map a raw `section.fields` index to the
    /// display row that focuses it. For Models the synthetic API-key row at
    /// `SYNTHETIC_KEY_ROW` shifts every field at or below it down by one, so
    /// the two mappings must stay in lockstep — `field_at_row(display_row_for_field(i)) == fields[i]`.
    pub(crate) fn display_row_for_field(section: &'static ConfigSectionMeta, fidx: usize) -> usize {
        match section.id {
            SectionId::Models if fidx >= SYNTHETIC_KEY_ROW => fidx + 1,
            _ => fidx,
        }
    }

    pub(crate) fn theme_row_at(&self, row: usize) -> Option<ThemeRow> {
        if self.current_section().id != SectionId::Themes {
            return None;
        }
        let names = crate::render::theme::available_theme_names(&self.effective);
        if row < names.len() {
            return Some(ThemeRow::Theme(names[row].clone()));
        }
        if row == names.len() {
            return Some(ThemeRow::New);
        }
        crate::render::theme::token_rows()
            .get(row.saturating_sub(names.len() + 1))
            .copied()
            .map(ThemeRow::Color)
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
        // The granular permission fields live under `[permissions.custom]`;
        // count ownership there too, or Space-cycle on Repo/Local can never
        // advance past the first option for a value this tier actually set.
        let owns = tier.contains_path(field.toml_path)
            || permission_detail_read_path(field).is_some_and(|p| tier.contains_path(p));
        Some(owns)
    }

    pub(crate) fn displayed_value_and_source(
        &self,
        field: &FieldMeta,
    ) -> (FieldValue, FieldSource) {
        // Read-only info rows are computed from the resolved config, not a TOML
        // path — render the computed value verbatim.
        if matches!(field.kind, FieldKind::Info) {
            return ((field.get)(&self.effective), FieldSource::Default);
        }
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
        // Per-provider routing fields (`["providers","*",<key>]`) resolve
        // against the ACTIVE provider: show the resolved value (override →
        // global → built-in) via `get`, and report the badge from whether THIS
        // provider explicitly sets it in the active tab's chain.
        if let ["providers", "*", key] = field.toml_path {
            let value = (field.get)(&self.effective);
            let slug = active_provider_slug(&self.effective);
            let real: [&str; 3] = ["providers", slug.as_str(), key];
            for (src, tier) in chain {
                if let Some(t) = tier
                    && t.contains_path(&real)
                {
                    return (value, *src);
                }
            }
            return (value, FieldSource::Default);
        }
        // Per-model limit fields (`["model_limits","*",<key>]`) resolve against
        // the active `provider:model`: show the resolved value via `get`, badge
        // from whether THIS model's entry is set in the active tab's chain.
        if let ["model_limits", "*", key] = field.toml_path {
            let value = (field.get)(&self.effective);
            let map_key = self.effective.model_limit_key();
            let real: [&str; 3] = ["model_limits", map_key.as_str(), key];
            for (src, tier) in chain {
                if let Some(t) = tier
                    && t.contains_path(&real)
                {
                    return (value, *src);
                }
            }
            return (value, FieldSource::Default);
        }
        // AI-reviewer fields resolve against the running config so the row
        // reflects what will actually be used (the resolved reviewer model, the
        // active capability set) rather than a static, often-empty default. The
        // badge still reflects whether the active tab sets the value.
        if let ["permissions", "ai_reviewer", _] = field.toml_path {
            let value = (field.get)(&self.effective);
            for (src, tier) in chain {
                if let Some(t) = tier
                    && tier_value_at_path(t, field).is_some()
                {
                    return (value, *src);
                }
            }
            return (value, FieldSource::Default);
        }
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
        // Read-only info rows are computed from the resolved config.
        if matches!(field.kind, FieldKind::Info) {
            return ((field.get)(&self.effective), FieldSource::Default);
        }
        // Per-provider routing fields (`["providers","*",<key>]`) resolve
        // against the ACTIVE provider: show the resolved value (override →
        // global → built-in) and report whether THIS provider explicitly sets
        // it (so the badge reads e.g. "user" when overridden, else "default").
        if let ["providers", "*", key] = field.toml_path {
            let value = (field.get)(&self.effective);
            let slug = active_provider_slug(&self.effective);
            let real: [&str; 3] = ["providers", slug.as_str(), key];
            let chain = [
                (FieldSource::Repo, ConfigScope::Local, &self.sources.repo),
                (
                    FieldSource::Project,
                    ConfigScope::Repo,
                    &self.sources.project,
                ),
                (FieldSource::User, ConfigScope::User, &self.sources.user),
            ];
            for (src, owns_scope, tier) in chain {
                if Some(owns_scope) == skip {
                    continue;
                }
                if let Some(t) = tier
                    && t.contains_path(&real)
                {
                    return (value, src);
                }
            }
            return (value, FieldSource::Default);
        }
        if let ["model_limits", "*", key] = field.toml_path {
            let value = (field.get)(&self.effective);
            let map_key = self.effective.model_limit_key();
            let real: [&str; 3] = ["model_limits", map_key.as_str(), key];
            let chain = [
                (FieldSource::Repo, ConfigScope::Local, &self.sources.repo),
                (
                    FieldSource::Project,
                    ConfigScope::Repo,
                    &self.sources.project,
                ),
                (FieldSource::User, ConfigScope::User, &self.sources.user),
            ];
            for (src, owns_scope, tier) in chain {
                if Some(owns_scope) == skip {
                    continue;
                }
                if let Some(t) = tier
                    && t.contains_path(&real)
                {
                    return (value, src);
                }
            }
            return (value, FieldSource::Default);
        }
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
                    FieldKind::Info
                        | FieldKind::TableArray { .. }
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

    /// Compute the `name → reverted-value` reverts a session-wide Discard
    /// would apply, by diffing each plain field's current effective value
    /// against the value it would resolve to once every tier file is rolled
    /// back to its opening (baseline) bytes. Used by the Discard confirmation
    /// so the user confirms against impact, not just a file list.
    ///
    /// Only plain `toml_path` leaves are diffed — the same kinds the Reset
    /// preview skips (`Info` / `TableArray` / `ProviderSubTabs` / `Secret`,
    /// plus the runtime-keyed `["*"]` routing/limit rows whose value reads off
    /// the live config) are left out so the preview never reports a lossy diff.
    /// Env-shadowed fields are skipped — Discard can't move them.
    pub(crate) fn discard_preview(&self) -> Vec<DiscardPreviewEntry> {
        // Re-parse the opening bytes into a tier chain, mirroring the order in
        // `baseline` ([user, project, repo]). A file that failed to parse (or
        // didn't exist) contributes nothing, exactly like an absent tier.
        let parse = |bytes: Option<&Vec<u8>>| -> Option<squeezy_core::TierSource> {
            let text = std::str::from_utf8(bytes?.as_slice()).ok()?;
            let doc = text.parse::<toml_edit::DocumentMut>().ok()?;
            Some(squeezy_core::TierSource {
                path: PathBuf::new(),
                doc,
            })
        };
        let user = self.baseline.first().and_then(|(_, b)| parse(b.as_ref()));
        let project = self.baseline.get(1).and_then(|(_, b)| parse(b.as_ref()));
        let repo = self.baseline.get(2).and_then(|(_, b)| parse(b.as_ref()));
        // Highest priority first: repo (Local) → project (Repo) → user.
        let baseline_chain: [Option<&squeezy_core::TierSource>; 3] =
            [repo.as_ref(), project.as_ref(), user.as_ref()];

        let mut out: Vec<DiscardPreviewEntry> = Vec::new();
        for section in CONFIG_SECTIONS {
            if section.id == SectionId::Reset {
                continue;
            }
            for field in section.fields {
                if matches!(
                    field.kind,
                    FieldKind::Info
                        | FieldKind::TableArray { .. }
                        | FieldKind::ProviderSubTabs
                        | FieldKind::Secret { .. }
                ) {
                    continue;
                }
                // Runtime-keyed `["*"]` rows read their value off the live
                // config, not the tier docs, so a baseline-doc walk can't
                // represent them faithfully — leave them out of the preview.
                if matches!(
                    field.toml_path,
                    ["providers", "*", _] | ["model_limits", "*", _]
                ) {
                    continue;
                }
                let (before, before_src) = self.effective_value_full(field);
                if before_src == FieldSource::Env {
                    continue;
                }
                let after = baseline_chain
                    .into_iter()
                    .find_map(|tier| tier.and_then(|t| tier_value_at_path(t, field)))
                    .unwrap_or_else(field.default);
                if before != after {
                    out.push(DiscardPreviewEntry {
                        section_label: section.label,
                        field_label: field.label,
                        before: before.as_display(),
                        after: after.as_display(),
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

/// One revert the session-wide Discard would apply: `before` is the current
/// effective value, `after` is the value once every tier rolls back to its
/// opening bytes. Rendered as a sampled, capped preview in the Discard
/// confirmation so the user sees what changes, not just which files.
#[derive(Debug, Clone)]
pub(crate) struct DiscardPreviewEntry {
    pub section_label: &'static str,
    pub field_label: &'static str,
    pub before: String,
    pub after: String,
}

/// Parse the `FieldValue` for `field` out of a tier's `DocumentMut`.
/// Returns `None` when the field is unset in this tier or when the leaf
/// type can't be represented in the current schema (e.g. `TableArray` /
/// `ProviderSubTabs`).
///
/// The ten granular permission fields are written to (and so read back
/// from) `[permissions.custom].<field>`; probe that location first and
/// only fall back to the legacy top-level `field.toml_path` so files
/// authored before the `custom` subtable existed still display.
fn tier_value_at_path(tier: &squeezy_core::TierSource, field: &FieldMeta) -> Option<FieldValue> {
    if let Some(custom_path) = permission_detail_read_path(field)
        && let Some(val) = tier_value_at_explicit_path(tier, custom_path, field.kind)
    {
        return Some(val);
    }
    tier_value_at_explicit_path(tier, field.toml_path, field.kind)
}

/// Parse the `FieldValue` at an explicit TOML `path` out of a tier's
/// `DocumentMut`, interpreting the leaf under `kind`. Returns `None` when
/// the path is unset or the leaf type isn't representable in the schema.
fn tier_value_at_explicit_path(
    tier: &squeezy_core::TierSource,
    path: &[&str],
    kind: FieldKind,
) -> Option<FieldValue> {
    if path.is_empty() {
        return None;
    }
    let (leaf, parents) = path.split_last().unwrap();
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
    match kind {
        FieldKind::Bool => value.as_bool().map(FieldValue::Bool),
        FieldKind::Integer { .. } => value.as_integer().map(FieldValue::Integer),
        FieldKind::OptionalInteger { .. } => value
            .as_integer()
            .map(|v| FieldValue::OptionalInteger(Some(v))),
        FieldKind::OptionalFloat { .. } => value
            .as_float()
            .or_else(|| value.as_integer().map(|v| v as f64))
            .map(|v| FieldValue::OptionalFloat(Some(v))),
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
        FieldKind::Info
        | FieldKind::Secret { .. }
        | FieldKind::ProviderSubTabs
        | FieldKind::TableArray { .. } => None,
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
/// hide the screen; `KeepOpen` keeps it; `OpenStatusLineSetup` asks the host to
/// open the rich `/statusline` builder over the (still-open) config screen, so
/// editing the `status_line` field uses the same discoverable picker as the
/// `/statusline` command instead of a raw comma-separated text editor.
pub(crate) enum KeyOutcome {
    KeepOpen,
    Close,
    OpenStatusLineSetup,
}

/// Action staged by the synchronous `/mcp` page key handler that the
/// outer async TUI loop drains and dispatches to the agent. We keep
/// the dispatch out-of-band so the per-key path stays sync; the loop
/// then runs `agent.set_mcp_server_enabled` / `restart_mcp_server` /
/// `replace_mcp_servers` (`squeezy-agent`) and refreshes the cached
/// snapshot via `refresh_mcp_screen_state`.
#[derive(Debug, Clone)]
pub(crate) enum McpAction {
    /// Toggle `enabled`. When `persist = true`, the change is written
    /// to settings TOML in addition to applying live; otherwise it
    /// stays session-only and reverts on restart.
    Toggle {
        server: String,
        enabled: bool,
        persist: bool,
    },
    /// Tear down the live session and rediscover tools.
    Restart { server: String },
    /// Insert a brand-new server. Persisted to settings TOML unless
    /// `persist = false` (session-only); always applied live. The
    /// `server` body is boxed because `McpServerConfig` is large
    /// (~330 bytes) and dominates the enum's size otherwise.
    Add {
        name: String,
        server: Box<squeezy_core::McpServerConfig>,
        persist: bool,
    },
    /// Drop a configured server. Persisted to TOML by default; the
    /// session-only path leaves the file untouched but removes the
    /// entry from the live registry.
    Remove { name: String, persist: bool },
}

/// Draft state for the "add server" overlay on the `/mcp` page. The
/// fields mirror the `[mcp.servers.<name>]` TOML block but trim to
/// the values needed to bring a stdio/http/sse server up. Persisted
/// fields are gated by the `transport` selector at submit time.
#[derive(Debug, Clone, Default)]
pub(crate) struct McpAddForm {
    pub name: String,
    pub transport: McpAddTransport,
    pub command: String,
    pub url: String,
    pub field_index: usize,
    /// Free-form error rendered above the form when the most recent
    /// submit attempt failed validation.
    pub error: Option<String>,
    /// When `true`, submit dispatches a session-only Add (no TOML
    /// write). Default is persisted.
    pub session_only: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum McpAddTransport {
    #[default]
    Stdio,
    Http,
    Sse,
}

impl McpAddTransport {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
            Self::Sse => "sse",
        }
    }

    pub(crate) fn next(self) -> Self {
        match self {
            Self::Stdio => Self::Http,
            Self::Http => Self::Sse,
            Self::Sse => Self::Stdio,
        }
    }

    /// Every transport choice, in cycle order — rendered as an inline
    /// option set so the row reads as a selector, not a text field.
    pub(crate) fn all() -> [Self; 3] {
        [Self::Stdio, Self::Http, Self::Sse]
    }
}

/// Row indices on the add-server overlay; the field index advances
/// through these and the `transport` selector renders inline so the
/// arrow keys cycle through every editable knob.
pub(crate) const MCP_ADD_FIELD_COUNT: usize = 4;

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
        (FieldKind::Integer { min, max, .. }, FieldValue::Integer(v)) => {
            let draft = v.to_string();
            let cursor = draft.len();
            FieldEditor::Integer {
                draft,
                cursor,
                min,
                max,
            }
        }
        (FieldKind::Integer { min, max, .. }, _) => FieldEditor::Integer {
            draft: String::new(),
            cursor: 0,
            min,
            max,
        },
        (FieldKind::OptionalInteger { min, max, .. }, FieldValue::OptionalInteger(Some(v))) => {
            let draft = v.to_string();
            let cursor = draft.len();
            FieldEditor::OptionalInteger {
                draft,
                cursor,
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
        (FieldKind::OptionalFloat { min, max }, FieldValue::OptionalFloat(Some(v))) => {
            let draft = format_editor_float(v);
            let cursor = draft.len();
            FieldEditor::OptionalFloat {
                draft,
                cursor,
                min,
                max,
            }
        }
        (FieldKind::OptionalFloat { min, max }, _) => FieldEditor::OptionalFloat {
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
        (FieldKind::DurationMs, FieldValue::Duration(d)) => {
            let draft = d.as_millis().to_string();
            let cursor = draft.len();
            FieldEditor::Duration { draft, cursor }
        }
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
        // Info / Secret / ProviderSubTabs / TableArray are not opened here —
        // Info is read-only, the others drop into dedicated sub-modes handled
        // by `handle_key`. Reaching this arm means a kind slipped past the
        // editability gate; fall back to a harmless empty text editor.
        (
            FieldKind::Info
            | FieldKind::Secret { .. }
            | FieldKind::ProviderSubTabs
            | FieldKind::TableArray { .. },
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
        FieldEditor::OptionalFloat {
            draft,
            cursor,
            min,
            max,
        } => float_editor_key(draft, cursor, *min, *max, key, true),
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

fn float_editor_key(
    draft: &mut String,
    cursor: &mut usize,
    min: f64,
    max: f64,
    key: KeyEvent,
    optional: bool,
) -> EditorOutcome {
    match key.code {
        KeyCode::Enter => {
            if optional && draft.trim().is_empty() {
                return EditorOutcome::Commit(FieldValue::OptionalFloat(None));
            }
            match draft.trim().parse::<f64>() {
                Ok(v) if v.is_finite() && (min..=max).contains(&v) => {
                    if optional {
                        EditorOutcome::Commit(FieldValue::OptionalFloat(Some(v)))
                    } else {
                        EditorOutcome::KeepEditing
                    }
                }
                Ok(_) | Err(_) => EditorOutcome::KeepEditing,
            }
        }
        KeyCode::Char(c) if c.is_ascii_digit() || c == '-' || c == '.' => {
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

fn format_editor_float(value: f64) -> String {
    let mut formatted = format!("{value:.6}");
    while formatted.contains('.') && formatted.ends_with('0') {
        formatted.pop();
    }
    if formatted.ends_with('.') {
        formatted.push('0');
    }
    formatted
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

/// Canonical slug of the active provider (the key into `[providers.<name>]`),
/// read off the Models `[model].provider` field — the same lookup the model
/// picker and the per-provider save path use.
pub(crate) fn active_provider_slug(cfg: &AppConfig) -> String {
    squeezy_core::config_schema::CONFIG_SECTIONS
        .iter()
        .find(|s| s.id == squeezy_core::config_schema::SectionId::Models)
        .and_then(|s| {
            s.fields
                .iter()
                .find(|f| f.toml_path == ["model", "provider"])
        })
        .map(|pf| match (pf.get)(cfg) {
            squeezy_core::config_schema::FieldValue::Enum(s) => s.to_string(),
            other => other.as_display(),
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "openai".to_string())
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
        P::GitHubCopilot(_) => "github_copilot",
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
        // Bedrock uses AWS SDK creds; OAuth providers store tokens at
        // `~/.squeezy/auth/`; the faux provider runs in-process and
        // has no credential. None of these have an env-var keychain
        // entry the screen can write.
        P::Bedrock(_) | P::OpenAiCodex(_) | P::GitHubCopilot(_) | P::Faux(_) => None,
        // Ollama can optionally use a bearer token for Cloud / reverse-proxy
        // deployments. Surface the key entry when api_key_env is set.
        P::Ollama(c) => {
            if c.api_key_env.is_empty() {
                None
            } else {
                Some(("Ollama", c.api_key_env.clone()))
            }
        }
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
        // OAuth provider credentials live in auth token files, not
        // provider TOML tables. The faux provider exposes `script`
        // instead of an api_key, which is handled by the field-level
        // editor rather than the secret-entry path.
        P::Bedrock(_) | P::OpenAiCodex(_) | P::GitHubCopilot(_) | P::Faux(_) => None,
        // Ollama supports an optional inline api_key for Cloud / reverse-proxy
        // deployments; write it under [providers.ollama] like any key-bearing provider.
        P::Ollama(c) => {
            if c.api_key_env.is_empty() {
                None
            } else {
                Some("ollama")
            }
        }
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
        P::Bedrock(_) | P::OpenAiCodex(_) | P::GitHubCopilot(_) | P::Faux(_) => None,
        P::Ollama(c) => c.api_key.clone(),
        P::OpenAiCompatible(c) => c.api_key.clone(),
    }
}

// ─── Live filter ───────────────────────────────────────────────────────────

/// Minimum query length before the filter narrows the list. Below this the
/// box stays open (showing what was typed) and the list is the full panel
/// index. Name matching activates on the first keystroke (a single-character
/// name substring like `p` → `permissions`/`provider` is highly selective);
/// the looser value/description channels stay gated by [`HELP_MIN_QUERY`].
pub(crate) const FILTER_MIN_QUERY: usize = 1;

/// Minimum query length before description (help-text) matching kicks in.
/// Help matching is a permissive substring test, and a two-character query is
/// a substring of nearly every blurb — so at 2 chars the filter narrows by
/// name only, and descriptions join in once the query is specific enough to
/// be meaningful.
const HELP_MIN_QUERY: usize = 3;

// Score bands (lower is better). An option-name hit ranks by the match's
// position in the label (`0` = prefix). A panel-name hit, a value hit, and a
// description hit fall into successively higher bands so they rank below
// option-name hits, in that order.
const SECTION_SCORE_BASE: i32 = 100;
const VALUE_SCORE_BASE: i32 = 1_000;
const DESC_SCORE: i32 = 10_000;

/// Rank everything on the screen against `query` by literal, case-insensitive
/// substring — page (section) names, option names, descriptions, and each
/// option's current value, so "show me the rows containing what I typed".
/// Substring (not subsequence) matching keeps it predictable: `sonnet` finds
/// the model whose value is `claude-sonnet-4-6` but not `reasoning_effort`
/// (where those letters only appear out of order).
///
/// Below [`FILTER_MIN_QUERY`] characters this returns the panel index (one
/// section-level entry per section) so the just-opened box reads as a list of
/// panels. Option names match from [`FILTER_MIN_QUERY`]; value, description, and
/// panel blurbs join from [`HELP_MIN_QUERY`] (a two-character substring appears
/// in too many values/blurbs to be useful). Ranking, best first: option name
/// (by position) → panel name → value → description. Typing a panel name
/// surfaces that panel's options. `effective` supplies the values. The result
/// is uncapped — the list scrolls and ranking surfaces the best matches first.
pub(crate) fn compute_search_matches(effective: &AppConfig, query: &str) -> Vec<SearchMatch> {
    let query_len = query.chars().count();
    if query_len < FILTER_MIN_QUERY {
        return (0..CONFIG_SECTIONS.len())
            .map(|section_index| SearchMatch {
                section_index,
                target: SearchTarget::Section,
                score: 0,
            })
            .collect();
    }
    let q = query.to_lowercase();
    let match_secondary = query_len >= HELP_MIN_QUERY;
    let mut out: Vec<SearchMatch> = Vec::new();
    for (sidx, section) in CONFIG_SECTIONS.iter().enumerate() {
        // Field-less sections (Themes / McpServers / Reset) render custom UI
        // and have no `FieldMeta` rows, so they're only reachable by matching
        // the section's own name or — once specific enough — a word in its blurb.
        if section.fields.is_empty() {
            let score = section
                .label
                .to_lowercase()
                .find(&q)
                .map(|p| p as i32)
                .or_else(|| {
                    (match_secondary && section.description.to_lowercase().contains(&q))
                        .then_some(DESC_SCORE)
                });
            if let Some(score) = score {
                out.push(SearchMatch {
                    section_index: sidx,
                    target: SearchTarget::Section,
                    score,
                });
            }
            continue;
        }
        for (fidx, field) in section.fields.iter().enumerate() {
            // Best of: the option name (ranked by position), the panel name
            // (surfaces every option when the panel is named), the current value
            // (e.g. typing `anthropic` finds the provider), or the description.
            let score = score_field(section, field, &q, match_secondary, effective);
            if let Some(score) = score {
                out.push(SearchMatch {
                    section_index: sidx,
                    target: SearchTarget::Field(fidx),
                    score,
                });
            }
        }
        // The Models section also exposes a synthetic API-key row with no
        // `FieldMeta` (it edits the active provider's credential), so the field
        // loop above misses it. Match it by name, the obvious credential
        // synonyms, or the active provider so `api_key` / `key` / `anthropic`
        // find it.
        if section.id == SectionId::Models {
            let score = "api_key".find(&q).map(|p| p as i32).or_else(|| {
                if !match_secondary {
                    return None;
                }
                active_provider_slug(effective)
                    .to_lowercase()
                    .find(&q)
                    .map(|p| VALUE_SCORE_BASE + p as i32)
                    .or_else(|| {
                        "api key credential secret"
                            .contains(&q)
                            .then_some(DESC_SCORE)
                    })
            });
            if let Some(score) = score {
                out.push(SearchMatch {
                    section_index: sidx,
                    target: SearchTarget::SyntheticApiKey,
                    score,
                });
            }
        }
    }
    // Lower is better; the sort is stable, so equal-scored rows keep schema
    // order and the list doesn't jitter as the user types.
    out.sort_by_key(|m| m.score);
    out
}

/// Best substring match score for a real field against the lowercased query
/// `q`. See [`compute_search_matches`] for the bands. `None` if nothing matches.
fn score_field(
    section: &ConfigSectionMeta,
    field: &FieldMeta,
    q: &str,
    match_secondary: bool,
    effective: &AppConfig,
) -> Option<i32> {
    if let Some(pos) = field.label.to_lowercase().find(q) {
        return Some(pos as i32);
    }
    // The panel name (e.g. typing `telemetry` lists every Telemetry option).
    if format!("{} {}", section.label, field.label)
        .to_lowercase()
        .contains(q)
    {
        return Some(SECTION_SCORE_BASE);
    }
    if !match_secondary {
        return None;
    }
    if let Some(pos) = (field.get)(effective).as_display().to_lowercase().find(q) {
        return Some(VALUE_SCORE_BASE + pos as i32);
    }
    field.help.to_lowercase().contains(q).then_some(DESC_SCORE)
}

/// Char indices in `haystack` matched by an ASCII-case-insensitive, greedy,
/// forward subsequence scan for `query`. A name/value match in
/// [`compute_search_matches`] implies the query is a subsequence of that text,
/// so this recovers which characters to emphasise. Returns `None` when `query`
/// is not a subsequence (e.g. the row matched on its help text, not its label),
/// so the renderer leaves that label unhighlighted.
pub(crate) fn subsequence_match_positions(haystack: &str, query: &str) -> Option<Vec<usize>> {
    let mut query_chars = query.chars();
    let mut want = query_chars.next();
    if want.is_none() {
        return Some(Vec::new());
    }
    let mut positions = Vec::new();
    for (idx, hc) in haystack.chars().enumerate() {
        if let Some(qc) = want
            && hc.eq_ignore_ascii_case(&qc)
        {
            positions.push(idx);
            want = query_chars.next();
            if want.is_none() {
                break;
            }
        }
    }
    want.is_none().then_some(positions)
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
