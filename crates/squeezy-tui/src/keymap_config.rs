//! Typed loader for the optional `~/.squeezy/keybindings.toml` file.
//!
//! Step `F03-keybindings-registry` (audit `tui-003` follow-up): the
//! resolver in `keymap.rs` already supports `[tui.keymap]` slug→keyspec
//! overrides loaded from `settings.toml`, but power users want a
//! dedicated keymap file that they can hand-edit without touching the
//! rest of their TUI settings. This module is the typed front-door for
//! that file:
//!
//! ```toml
//! # ~/.squeezy/keybindings.toml
//! [[bindings]]
//! key = "Ctrl+o"
//! action = "transcript_overlay"
//!
//! [[bindings]]
//! key = "Alt+k"
//! action = "page_up"
//! ```
//!
//! Each entry deserialises into [`KeyBinding`] (`{ key: String, action:
//! KeymapAction }`) where `action` is validated against the existing
//! [`Action::from_slug`] table. After deserialisation we re-parse the
//! `key` field through [`crate::keymap::parse_keyspec`] so the same
//! "Ctrl+", "Alt+", function-key, special-key syntax that `[tui.keymap]`
//! uses applies here too — there is exactly one keyspec grammar in the
//! crate.
//!
//! Reserved bindings (`Ctrl+C`, `Esc`, `Ctrl+D`) are rejected with a
//! typed error rather than silently overridden. These are the only ways
//! the user can leave the TUI in an emergency (turn-cancel, dismiss
//! overlay, EOF/composer-exit); silently rebinding them would trap the
//! user with no exit hatch, so the loader refuses the override.
//!
//! The merged result feeds back into [`crate::keymap::KeymapResolver`]
//! as a slug→keyspec map so the rest of the TUI keeps a single
//! resolver type and `/keymap` continues to render every binding.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyModifiers};
use serde::{Deserialize, Deserializer};

use crate::keymap::{Action, KeyBinding as ResolvedKeyBinding, parse_keyspec};

/// Re-export of the typed action enum under the name documented in the
/// keybindings file (`action = "<slug>"`). Keeping it as an alias means
/// the slug list stays sourced from [`Action::from_slug`] — no second
/// table to keep in sync.
pub(crate) type KeymapAction = Action;

/// One row from the `[[bindings]]` array of `~/.squeezy/keybindings.toml`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct KeyBinding {
    /// Keyspec string in the same grammar `[tui.keymap]` accepts
    /// (`"Ctrl+T"`, `"PageUp"`, `"Alt+k"`, …). Parsed by
    /// [`parse_keyspec`] when the file is validated.
    pub(crate) key: String,
    /// Action slug. Deserialised through [`deserialize_action`] so
    /// unknown slugs fail at parse time instead of being silently
    /// dropped.
    #[serde(deserialize_with = "deserialize_action")]
    pub(crate) action: KeymapAction,
}

fn deserialize_action<'de, D>(deserializer: D) -> Result<KeymapAction, D::Error>
where
    D: Deserializer<'de>,
{
    let slug = String::deserialize(deserializer)?;
    Action::from_slug(&slug)
        .ok_or_else(|| serde::de::Error::custom(format!("unknown action slug {slug:?}")))
}

/// The TOML root: `bindings = [...]`. `#[serde(default)]` lets an empty
/// file (`""`) parse as "no overrides" instead of an error, matching
/// what users expect when they create the file before adding rows.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct KeybindingsFile {
    #[serde(default)]
    pub(crate) bindings: Vec<KeyBinding>,
}

/// Reserved key/modifier pairs that the user cannot rebind. These are
/// the TUI's emergency exits: `Ctrl+C` cancels the running turn (and
/// exits when idle), `Esc` dismisses overlays / aborts streams, and
/// `Ctrl+D` closes the composer like a shell EOF. Silently rebinding
/// any of them would strand the user with no way out, so the loader
/// surfaces a typed error instead of accepting the override.
const RESERVED_BINDINGS: &[(&str, KeyCode, KeyModifiers)] = &[
    ("Ctrl+C", KeyCode::Char('c'), KeyModifiers::CONTROL),
    ("Esc", KeyCode::Esc, KeyModifiers::NONE),
    ("Ctrl+D", KeyCode::Char('d'), KeyModifiers::CONTROL),
];

/// Return the user-facing label for the reserved binding that `parsed`
/// matches, or `None` if the binding is not reserved. Character codes
/// compare case-insensitively so both `"Ctrl+C"` and `"Ctrl+c"` are
/// caught.
pub(crate) fn reserved_label(parsed: &ResolvedKeyBinding) -> Option<&'static str> {
    for (label, code, mods) in RESERVED_BINDINGS {
        if parsed.modifiers != *mods {
            continue;
        }
        let matches = match (parsed.code, *code) {
            (KeyCode::Char(a), KeyCode::Char(b)) => a.eq_ignore_ascii_case(&b),
            (a, b) => a == b,
        };
        if matches {
            return Some(*label);
        }
    }
    None
}

/// Validation/loading failures surfaced by this module. Returned as a
/// single error so callers can `tracing::warn!` once and fall back to
/// the compiled-in defaults rather than half-applying a broken file.
#[derive(Debug)]
pub(crate) enum KeybindingsError {
    /// Could not read the file at `path` (permissions, transient IO,
    /// …). File-not-found is handled upstream by [`merge_user_overrides`]
    /// before reaching `load`, so this variant indicates a real
    /// problem worth surfacing.
    Io { path: PathBuf, source: io::Error },
    /// TOML deserialisation failed. `path` is `Some` when triggered by
    /// [`KeybindingsFile::load`] and `None` for the in-memory
    /// [`KeybindingsFile::from_toml_str`] path used in tests.
    Parse {
        path: Option<PathBuf>,
        source: toml::de::Error,
    },
    /// The `key` field of a `[[bindings]]` row did not parse as a
    /// valid keyspec (e.g. `"totally-not-a-key"`). Carries the action
    /// so the message can pinpoint which row needs fixing.
    InvalidKeyspec { action: KeymapAction, key: String },
    /// The user tried to rebind some action onto a reserved key
    /// (`Ctrl+C`, `Esc`, `Ctrl+D`). `reserved` is the label of the
    /// matched reserved binding.
    ReservedKey {
        action: KeymapAction,
        key: String,
        reserved: &'static str,
    },
}

impl fmt::Display for KeybindingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            Self::Parse {
                path: Some(path),
                source,
            } => write!(f, "failed to parse {}: {source}", path.display()),
            Self::Parse { path: None, source } => {
                write!(f, "failed to parse keybindings TOML: {source}")
            }
            Self::InvalidKeyspec { action, key } => {
                write!(f, "invalid keyspec {key:?} for action {:?}", action.slug())
            }
            Self::ReservedKey {
                action,
                key,
                reserved,
            } => write!(
                f,
                "cannot rebind action {:?} to {key:?}: {reserved} is reserved",
                action.slug()
            ),
        }
    }
}

impl std::error::Error for KeybindingsError {}

impl KeybindingsFile {
    /// Deserialise a `[[bindings]]` document from an in-memory string.
    /// Test-only today; if/when the settings watcher gains live reload
    /// for this file, drop the `#[cfg(test)]` gate so it can drive
    /// the parse path without an intermediate temp file.
    #[cfg(test)]
    pub(crate) fn from_toml_str(content: &str) -> Result<Self, KeybindingsError> {
        toml::from_str(content).map_err(|source| KeybindingsError::Parse { path: None, source })
    }

    /// Read and parse the keybindings file at `path`. Caller is
    /// expected to handle the "file does not exist" case upstream;
    /// see [`merge_user_overrides`].
    pub(crate) fn load(path: &Path) -> Result<Self, KeybindingsError> {
        let content = std::fs::read_to_string(path).map_err(|source| KeybindingsError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&content).map_err(|source| KeybindingsError::Parse {
            path: Some(path.to_path_buf()),
            source,
        })
    }

    /// Validate every binding (re-parse the keyspec, reject reserved
    /// keys) and produce the slug→keyspec map that
    /// [`crate::keymap::KeymapResolver::from_overrides`] consumes.
    ///
    /// Returns the first validation failure rather than collecting:
    /// misconfiguring a personal keymap file should be loud and
    /// all-or-nothing, not silently partially-applied.
    pub(crate) fn into_override_map(self) -> Result<BTreeMap<String, String>, KeybindingsError> {
        let mut out = BTreeMap::new();
        for KeyBinding { key, action } in self.bindings {
            let parsed = parse_keyspec(&key).ok_or_else(|| KeybindingsError::InvalidKeyspec {
                action,
                key: key.clone(),
            })?;
            if let Some(reserved) = reserved_label(&parsed) {
                return Err(KeybindingsError::ReservedKey {
                    action,
                    key,
                    reserved,
                });
            }
            out.insert(action.slug().to_string(), key);
        }
        Ok(out)
    }
}

/// Default location of the user-editable file:
/// `~/.squeezy/keybindings.toml`. Returns `None` when neither `$HOME`
/// nor a platform home directory (e.g. `USERPROFILE` on Windows) is
/// resolvable — CI sandboxes, some test harnesses — in which case the
/// loader degrades to "no user overrides".
///
/// Uses `squeezy_core::cached_home_dir()` so the Windows profile-directory
/// lookup is cached for the process lifetime.
pub(crate) fn default_keybindings_path() -> Option<PathBuf> {
    let home = squeezy_core::cached_home_dir()?;
    Some(home.join(".squeezy").join("keybindings.toml"))
}

/// Merge `~/.squeezy/keybindings.toml` (when present) on top of the
/// already-resolved `[tui.keymap]` overrides from `settings.toml`.
///
/// Precedence: the user file wins on any slug it specifies; entries
/// from `base` that the file does not touch survive unchanged. A
/// missing path, a missing file, or `None` is treated as "no user
/// overrides" and returns `base` verbatim — that keeps the
/// pre-`F03` default key behavior intact for users who never create
/// the file.
pub(crate) fn merge_user_overrides(
    base: BTreeMap<String, String>,
    user_path: Option<&Path>,
) -> Result<BTreeMap<String, String>, KeybindingsError> {
    let mut merged = base;
    let Some(path) = user_path else {
        return Ok(merged);
    };
    if !path.exists() {
        return Ok(merged);
    }
    let file = KeybindingsFile::load(path)?;
    for (slug, spec) in file.into_override_map()? {
        merged.insert(slug, spec);
    }
    Ok(merged)
}

#[cfg(test)]
#[path = "keymap_config_tests.rs"]
mod tests;
