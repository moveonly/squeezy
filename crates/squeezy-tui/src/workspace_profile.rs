//! Per-Workspace UI Profile (§12.7.4).
//!
//! Remembers a small set of UI preferences — transcript density, transcript
//! detail default, the minimap pane's visibility, and the active theme — per
//! workspace (resolved repo root), restoring them the next time the TUI launches
//! in that workspace. The profile is stored OUTSIDE the repo, under the existing
//! Squeezy state/config hierarchy (the same per-project area
//! [`squeezy_core::default_projects_dir`] backs), keyed by a stable workspace
//! identity hash ([`squeezy_core::repo_settings_id`]) so two workspaces never
//! leak preferences into each other and the worktree is never dirtied.
//!
//! ## Model, not chrome
//!
//! Like its peer leaf modules ([`crate::theme_editor`], [`crate::snippet_store`])
//! this file owns only the *pure* model and the *pure* persistence math:
//!
//!   - [`UiProfile`]: the serialized, schema-versioned snapshot of the four
//!     remembered preferences (each optional, so an absent field falls through to
//!     the global config default rather than overriding it).
//!   - [`ProfileField`] / [`FIELDS`]: the ordered, user-facing rows the overlay
//!     lists, so a focus move and a click target both index the same list.
//!   - [`WorkspaceProfileState`]: the focused row plus the source label, the
//!     overlay's terminal-free, fully unit-testable core.
//!   - [`profile_path`] / [`load`] / [`save`] / [`clear`]: resolve the on-disk
//!     path for a workspace root and round-trip the profile through TOML.
//!
//! `lib.rs` owns the side effects: the keybinding, the open/close flag, the
//! per-frame render call through the single fullscreen `render()`, capturing the
//! live app state into a [`UiProfile`], applying a loaded profile back onto the
//! running app + agent config, and the save/reset verbs.
//!
//! ## Bounds & idle cost
//!
//! The field list is a compile-time constant; the overlay state is one cursor and
//! a short label string. The overlay is closed by default (a single `Option` on
//! `TuiApp`) and at rest paints nothing and schedules no redraw, so an idle
//! session pays one enum-tag check and nothing more. Restore-on-launch reads one
//! small TOML file once at startup and never again.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use squeezy_core::{ToolOutputVerbosity, TranscriptDefault};

/// Schema version stamped into every persisted [`UiProfile`]. Bumped only when
/// the on-disk shape changes incompatibly; a file with an unrecognised (newer)
/// version is ignored on load rather than misread, so a downgrade never corrupts
/// the running session.
pub(crate) const PROFILE_SCHEMA_VERSION: u32 = 1;

/// Environment variable that, when set, overrides the directory the per-workspace
/// UI profiles are stored under. Lets the eval harness and the unit tests pin the
/// store to a scratch directory so a test never reads or writes the real
/// `~/.squeezy/projects` tree. Production sessions never set it and fall through
/// to [`squeezy_core::default_projects_dir`].
pub(crate) const PROFILE_DIR_ENV: &str = "SQUEEZY_UI_PROFILE_DIR";

/// The serialized, schema-versioned snapshot of a workspace's remembered UI
/// preferences. Every preference is `Option`: `None` means "not remembered — fall
/// through to the global config default" so an unset field never forces a value
/// onto a workspace the user has not customized there. Persisted as a small TOML
/// file outside the repo.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UiProfile {
    /// On-disk schema version (see [`PROFILE_SCHEMA_VERSION`]). Defaults to 0 for
    /// a hand-written or legacy file missing the key; [`load`] tolerates 0 and the
    /// current version and rejects anything newer.
    #[serde(default)]
    pub(crate) version: u32,
    /// Remembered transcript/tool-output density (the `/verbosity` knob).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) density: Option<ToolOutputVerbosity>,
    /// Remembered transcript expansion default (compact vs expanded entries).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) transcript_detail: Option<TranscriptDefault>,
    /// Remembered minimap pane visibility (the right-rail overview pane).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) minimap: Option<bool>,
    /// Remembered active theme name (e.g. `"catppuccin"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) theme: Option<String>,
}

impl UiProfile {
    /// A freshly-captured profile carries the current schema version so a later
    /// load can tell which writer produced it.
    pub(crate) fn new(
        density: ToolOutputVerbosity,
        transcript_detail: TranscriptDefault,
        minimap: bool,
        theme: String,
    ) -> Self {
        Self {
            version: PROFILE_SCHEMA_VERSION,
            density: Some(density),
            transcript_detail: Some(transcript_detail),
            minimap: Some(minimap),
            theme: Some(theme),
        }
    }

    /// True when the profile remembers no preference at all — the resting state of
    /// a workspace that has never been customized. The overlay shows a hint and
    /// `restore`-on-launch short-circuits.
    pub(crate) fn is_empty(&self) -> bool {
        self.density.is_none()
            && self.transcript_detail.is_none()
            && self.minimap.is_none()
            && self.theme.is_none()
    }
}

/// One row in the workspace-profile overlay. The overlay lists exactly these, in
/// order, so a `↑/↓` focus move and a click on a painted row both index
/// [`FIELDS`] identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProfileField {
    /// Transcript/tool-output density.
    Density,
    /// Transcript expansion default.
    TranscriptDetail,
    /// Minimap pane visibility.
    Minimap,
    /// Active theme name.
    Theme,
}

impl ProfileField {
    /// Short, friendly row label shown in the overlay.
    pub(crate) fn label(self) -> &'static str {
        match self {
            ProfileField::Density => "Density",
            ProfileField::TranscriptDetail => "Transcript detail",
            ProfileField::Minimap => "Minimap pane",
            ProfileField::Theme => "Theme",
        }
    }

    /// Render the field's current value from a captured [`UiProfile`] as a short
    /// human string. An unremembered (`None`) field reads as `"—"`.
    pub(crate) fn value_str(self, profile: &UiProfile) -> String {
        match self {
            ProfileField::Density => profile
                .density
                .map(|d| d.as_str().to_string())
                .unwrap_or_else(|| "\u{2014}".to_string()),
            ProfileField::TranscriptDetail => profile
                .transcript_detail
                .map(|t| t.as_str().to_string())
                .unwrap_or_else(|| "\u{2014}".to_string()),
            ProfileField::Minimap => match profile.minimap {
                Some(true) => "shown".to_string(),
                Some(false) => "hidden".to_string(),
                None => "\u{2014}".to_string(),
            },
            ProfileField::Theme => profile
                .theme
                .clone()
                .unwrap_or_else(|| "\u{2014}".to_string()),
        }
    }
}

/// The ordered, user-facing field rows the overlay exposes. A compile-time
/// constant so the overlay's navigation, render, and click hit-testing all index
/// the same list.
pub(crate) const FIELDS: &[ProfileField] = &[
    ProfileField::Density,
    ProfileField::TranscriptDetail,
    ProfileField::Minimap,
    ProfileField::Theme,
];

/// The pure overlay model. Holds the focused field row and the resolved
/// source-label string (e.g. the relative on-disk path the profile lives at) so
/// the overlay can report *where* the profile is stored without re-resolving the
/// path every frame. All persistence / app-apply side effects live in `lib.rs`;
/// this struct is the terminal-free, fully unit-testable core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceProfileState {
    /// Cursor into [`FIELDS`]. Always in bounds (the constructor and movers clamp
    /// it), so [`Self::current_field`] never panics.
    field: usize,
    /// Human-facing label for where this workspace's profile is stored, shown in
    /// the overlay header so the user can see the source path the spec asks for.
    source_label: String,
}

impl WorkspaceProfileState {
    /// Open the overlay focused on the first field, carrying the resolved source
    /// label for the header.
    pub(crate) fn new(source_label: String) -> Self {
        Self {
            field: 0,
            source_label,
        }
    }

    /// The focused field. Always valid: [`FIELDS`] is non-empty and `field` is
    /// clamped on every move.
    pub(crate) fn current_field(&self) -> ProfileField {
        FIELDS[self.field.min(FIELDS.len() - 1)]
    }

    /// Index of the focused field into [`FIELDS`].
    pub(crate) fn field_index(&self) -> usize {
        self.field.min(FIELDS.len() - 1)
    }

    /// The source-path label for the overlay header.
    pub(crate) fn source_label(&self) -> &str {
        &self.source_label
    }

    /// Move the field focus up one row (clamped at the top). Returns `true` when
    /// the focus actually moved.
    pub(crate) fn focus_prev(&mut self) -> bool {
        if self.field == 0 {
            return false;
        }
        self.field -= 1;
        true
    }

    /// Move the field focus down one row (clamped at the bottom). Returns `true`
    /// when the focus actually moved.
    pub(crate) fn focus_next(&mut self) -> bool {
        if self.field + 1 >= FIELDS.len() {
            return false;
        }
        self.field += 1;
        true
    }

    /// Focus a field directly by its [`FIELDS`] index (the mouse twin of `↑/↓`
    /// over a row). Out-of-range indices are ignored. Returns `true` when the
    /// focus actually moved.
    pub(crate) fn focus_field(&mut self, index: usize) -> bool {
        if index >= FIELDS.len() || index == self.field {
            return false;
        }
        self.field = index;
        true
    }
}

/// Resolve the directory the per-workspace profiles are stored under. Honours the
/// [`PROFILE_DIR_ENV`] override (tests / eval harness), else the production
/// [`squeezy_core::default_projects_dir`] (which itself maps to the platform's
/// state/config location: `~/.squeezy/projects`, `$XDG_CONFIG_HOME/squeezy/...`,
/// `%APPDATA%\squeezy\...`).
fn profile_root_dir() -> PathBuf {
    if let Some(custom) = std::env::var_os(PROFILE_DIR_ENV) {
        return PathBuf::from(custom);
    }
    squeezy_core::default_projects_dir()
}

/// Resolve the on-disk path of `root`'s UI profile: `<projects dir>/<workspace
/// id>/ui_profile.toml`. The workspace id is the same resolved-root hash
/// [`squeezy_core::repo_settings_id`] keys per-repo settings by, so the profile
/// rides the existing per-project state hierarchy and never lands inside the repo.
pub(crate) fn profile_path(root: impl AsRef<Path>) -> PathBuf {
    profile_root_dir()
        .join(squeezy_core::repo_settings_id(root))
        .join("ui_profile.toml")
}

/// A short, redaction-safe label for where `root`'s profile is stored: the
/// workspace id plus the file name, WITHOUT the absolute parent directory (which
/// can reveal the user's home path). Shown in the overlay header so the user can
/// see the source the spec calls for without leaking the full local path.
pub(crate) fn source_label(root: impl AsRef<Path>) -> String {
    format!(
        "projects/{}/ui_profile.toml",
        squeezy_core::repo_settings_id(root)
    )
}

/// Load `root`'s persisted profile, or [`UiProfile::default`] (all-`None`) when no
/// file exists, the file is unreadable/unparseable, or it carries a newer schema
/// version than this build understands. Never errors: a missing or malformed
/// profile is treated as "no preferences remembered" so a bad file can never block
/// launch.
pub(crate) fn load(root: impl AsRef<Path>) -> UiProfile {
    let path = profile_path(root);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return UiProfile::default();
    };
    match toml::from_str::<UiProfile>(&text) {
        Ok(profile) if profile.version <= PROFILE_SCHEMA_VERSION => profile,
        // A newer schema we don't understand, or a parse failure: ignore it rather
        // than misread it. The next save rewrites it in the current shape.
        _ => UiProfile::default(),
    }
}

/// Persist `profile` for `root`, creating the parent directory as needed. Returns
/// the path written on success. The file lives under the Squeezy projects state
/// dir, never inside the repo, so saving never dirties the worktree.
pub(crate) fn save(root: impl AsRef<Path>, profile: &UiProfile) -> std::io::Result<PathBuf> {
    let path = profile_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(profile)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    std::fs::write(&path, text)?;
    Ok(path)
}

/// Forget `root`'s profile by removing its on-disk file. A missing file is a
/// success (the workspace already has no remembered preferences), so a double
/// reset is harmless.
pub(crate) fn clear(root: impl AsRef<Path>) -> std::io::Result<()> {
    let path = profile_path(root);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
#[path = "workspace_profile_tests.rs"]
mod tests;
