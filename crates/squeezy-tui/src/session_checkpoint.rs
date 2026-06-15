//! Session Auto-Save Checkpoints For UI State (§12.9.5).
//!
//! Periodically auto-saves a small, logical snapshot of the live UI state —
//! scroll anchor, focused entry, the active search query, and the minimap
//! pane's visibility — keyed by session id, and restores it on relaunch / after
//! a crash so a session reopens exactly where the user left it. The spec calls
//! for *logical anchors, not coordinates*: the checkpoint stores a
//! `from_bottom` distance + a follow-tail flag (not an absolute pixel/row), a
//! transcript revision (the entry count) so a stale anchor is recognised, and
//! the selected-entry index validated and CLAMPED on load against the current
//! transcript.
//!
//! ## Model and checkpoint surface
//!
//! Like its peer leaf modules ([`crate::workspace_profile`],
//! [`crate::glyph_mode`]) this file owns the feature model, the persistence
//! math, and the small read-only status surface:
//!
//!   - [`UiStateCheckpoint`]: the serialized, schema-versioned snapshot.
//!   - [`CheckpointStore`]: the debounce gate + last-captured fingerprint. The
//!     gate is driven by an explicit `now: Instant` passed in by the caller and
//!     compared with [`Instant::saturating_duration_since`] — NEVER by
//!     back-dating an `Instant` with bare subtraction (which panics on a fresh
//!     Windows monotonic clock), so the test helpers construct past instants
//!     with `checked_sub` + a clock-safe fallback.
//!   - [`CheckpointUiState`]: the `TuiApp` grouping for the background store and
//!     the optional overlay snapshot.
//!   - [`render_surface`]: the status overlay painter for the checkpoint.
//!   - [`checkpoint_path`] / [`load`] / [`save`] / [`clear`]: resolve the
//!     on-disk path for a session id and round-trip the checkpoint through TOML
//!     with an atomic `<file>.tmp` → rename write.
//!
//! `lib.rs` owns the side effects: capturing the live app state into a
//! [`UiStateCheckpoint`], the debounced auto-save tick in the run loop, the
//! restore-on-launch hook, and the overlay keybinding.
//!
//! ## Bounds & idle cost
//!
//! The debounce only runs on iterations where the UI actually changed (the run
//! loop already computes `wants_draw`); an idle session pays nothing beyond the
//! one cheap fingerprint comparison those state-change iterations already
//! warrant. A captured checkpoint is a handful of small fields; the on-disk file
//! is a few lines of TOML written at most once per [`SAVE_DEBOUNCE`] window.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use serde::{Deserialize, Serialize};

use crate::{interaction, modal};

/// Schema version stamped into every persisted [`UiStateCheckpoint`]. Bumped
/// only when the on-disk shape changes incompatibly; a file with an
/// unrecognised (newer) version is ignored on load rather than misread, so a
/// downgrade never restores garbage into a running session.
pub(crate) const CHECKPOINT_SCHEMA_VERSION: u32 = 1;

/// Environment variable that, when set, overrides the directory the per-session
/// UI-state checkpoints are stored under. Lets the eval harness and the unit
/// tests pin the store to a scratch directory so a test never reads or writes
/// the real `~/.squeezy/projects` tree. Production sessions never set it and
/// fall through to [`squeezy_core::default_projects_dir`].
pub(crate) const CHECKPOINT_DIR_ENV: &str = "SQUEEZY_UI_CHECKPOINT_DIR";

/// Minimum wall-clock gap between two on-disk auto-saves. The debounce coalesces
/// a burst of scroll/search/selection changes into a single write, so rapid
/// interaction never turns into a write storm. Chosen generously (2s) because a
/// checkpoint only needs to survive a crash/suspend, not capture every keystroke.
pub(crate) const SAVE_DEBOUNCE: Duration = Duration::from_secs(2);

/// The serialized, schema-versioned snapshot of a session's UI state. Every
/// field is a LOGICAL anchor, never an absolute coordinate: the scroll position
/// is a `from_bottom` distance + follow flag, the focused entry is an index
/// validated against the transcript on load, and `transcript_revision` lets a
/// stale checkpoint (the transcript grew/shrank since) be recognised and clamped
/// rather than misapplied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UiStateCheckpoint {
    /// On-disk schema version (see [`CHECKPOINT_SCHEMA_VERSION`]). Defaults to 0
    /// for a hand-written or legacy file missing the key; [`load`] tolerates 0
    /// and the current version and rejects anything newer.
    #[serde(default)]
    pub(crate) version: u32,
    /// The session id this checkpoint belongs to. Restore matches it against the
    /// live session id so a checkpoint is never applied to a different session
    /// that happened to collide on a reused store path.
    #[serde(default)]
    pub(crate) session_id: String,
    /// Transcript revision at capture time — the entry count. A coarse but
    /// stable "how much had been said" marker: on load, a selected-entry index
    /// or scroll anchor past the CURRENT transcript is clamped rather than
    /// restored verbatim.
    #[serde(default)]
    pub(crate) transcript_revision: usize,
    /// Logical scroll anchor: lines scrolled up from the tail (`0` == following
    /// the tail). Clamped to the live max scroll on restore.
    #[serde(default)]
    pub(crate) scroll_from_bottom: usize,
    /// Whether the view was pinned to (following) the tail at capture. A
    /// following view restores to the tail regardless of `scroll_from_bottom`.
    #[serde(default)]
    pub(crate) following_tail: bool,
    /// The focused transcript entry index, if any. `None` == no entry focused
    /// (the tail-follow default). Clamped to the live transcript length on load;
    /// an out-of-range index restores as `None` rather than panicking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) selected_entry: Option<usize>,
    /// The active search query, if the search bar was open with a non-empty
    /// query. Kept local to the session store and omitted when empty so an idle
    /// session never persists a stray query. May be sensitive — honours the same
    /// local-only storage as the rest of the checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) search_query: Option<String>,
    /// The minimap pane's visibility (a "pinned pane" in the spec's terms) at
    /// capture, so a session reopens with the same right-rail overview state.
    #[serde(default)]
    pub(crate) show_minimap: bool,
}

impl UiStateCheckpoint {
    /// A freshly-captured checkpoint carries the current schema version so a
    /// later load can tell which writer produced it.
    pub(crate) fn new(
        session_id: String,
        transcript_revision: usize,
        scroll_from_bottom: usize,
        following_tail: bool,
        selected_entry: Option<usize>,
        search_query: Option<String>,
        show_minimap: bool,
    ) -> Self {
        Self {
            version: CHECKPOINT_SCHEMA_VERSION,
            session_id,
            transcript_revision,
            scroll_from_bottom,
            following_tail,
            // An empty query is the same as "no search" for restore purposes.
            search_query: search_query.filter(|q| !q.is_empty()),
            selected_entry,
            show_minimap,
        }
    }

    /// Validate + clamp this checkpoint against the CURRENT transcript length so
    /// a stale anchor (the transcript shrank since capture) is never restored out
    /// of range. Returns a copy safe to apply: the selected entry is dropped if
    /// it now points past the end, and the scroll anchor is clamped to a distance
    /// the live content can actually support. The scroll clamp here is the coarse
    /// "never beyond the entry count" bound; the geometry-aware final clamp
    /// happens at apply time through [`crate::scroll::ScrollState::set_from_bottom`].
    pub(crate) fn clamped_for(&self, transcript_len: usize) -> Self {
        let selected_entry = self.selected_entry.filter(|&index| index < transcript_len);
        // A following view ignores the stored distance; otherwise bound the
        // distance to the entry count so a wildly stale anchor can't ask to
        // scroll past all content.
        let scroll_from_bottom = if self.following_tail {
            0
        } else {
            self.scroll_from_bottom.min(transcript_len)
        };
        Self {
            selected_entry,
            scroll_from_bottom,
            ..self.clone()
        }
    }

    /// True when this checkpoint belongs to `session_id`. Restore short-circuits
    /// when it does not, so a checkpoint never leaks into the wrong session.
    pub(crate) fn matches_session(&self, session_id: &str) -> bool {
        self.session_id == session_id
    }
}

/// A cheap, comparable fingerprint of the UI state worth checkpointing. The
/// store keeps the last-saved fingerprint so an auto-save is skipped entirely
/// when nothing relevant changed since the previous write — the debounce only
/// gates writes that would actually differ.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    transcript_revision: usize,
    scroll_from_bottom: usize,
    following_tail: bool,
    selected_entry: Option<usize>,
    search_query: Option<String>,
    show_minimap: bool,
}

impl Fingerprint {
    fn of(checkpoint: &UiStateCheckpoint) -> Self {
        Self {
            transcript_revision: checkpoint.transcript_revision,
            scroll_from_bottom: checkpoint.scroll_from_bottom,
            following_tail: checkpoint.following_tail,
            selected_entry: checkpoint.selected_entry,
            search_query: checkpoint.search_query.clone(),
            show_minimap: checkpoint.show_minimap,
        }
    }
}

/// The debounce gate for the auto-save. Tracks the last on-disk write time and
/// the fingerprint that was written, so [`Self::should_save`] can answer "is this
/// candidate both DIFFERENT from the last save AND past the debounce window?"
/// without the caller juggling timing math. Time is always supplied by the
/// caller (`now: Instant`) and compared with [`Instant::saturating_duration_since`]
/// — the store never reads the clock or back-dates an `Instant` itself, which
/// keeps it deterministic in tests and free of the Windows monotonic-clock panic.
#[derive(Debug, Default)]
pub(crate) struct CheckpointStore {
    last_saved_at: Option<Instant>,
    last_saved: Option<Fingerprint>,
}

impl CheckpointStore {
    /// A fresh store with nothing saved yet — the first eligible change saves
    /// immediately (no debounce gate to clear on the very first write).
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Should `candidate` be written to disk right now? `true` only when the
    /// candidate's fingerprint DIFFERS from the last write AND either nothing has
    /// been written yet OR at least [`SAVE_DEBOUNCE`] has elapsed since the last
    /// write. A candidate identical to the last save is always skipped (no write
    /// churn for a redraw that didn't change any checkpointed field), regardless
    /// of elapsed time.
    pub(crate) fn should_save(&self, candidate: &UiStateCheckpoint, now: Instant) -> bool {
        let fingerprint = Fingerprint::of(candidate);
        if self.last_saved.as_ref() == Some(&fingerprint) {
            return false;
        }
        match self.last_saved_at {
            // saturating_duration_since never panics even if `now` precedes the
            // stored instant (a clock anomaly); it simply yields zero, which the
            // `>=` below treats as "not yet elapsed".
            Some(saved_at) => now.saturating_duration_since(saved_at) >= SAVE_DEBOUNCE,
            None => true,
        }
    }

    /// Record that `candidate` was just written at `now`, so the next
    /// [`Self::should_save`] gates against this fingerprint + time. Call only
    /// after a successful write so a failed save retries on the next eligible
    /// change instead of being silently swallowed.
    pub(crate) fn record_saved(&mut self, candidate: &UiStateCheckpoint, now: Instant) {
        self.last_saved = Some(Fingerprint::of(candidate));
        self.last_saved_at = Some(now);
    }

    /// Whether anything has been written this session yet — drives the overlay's
    /// "no checkpoint saved yet" hint.
    pub(crate) fn has_saved(&self) -> bool {
        self.last_saved.is_some()
    }
}

/// TuiApp-owned session-checkpoint state grouped under one feature field: the
/// debounced background store plus the optional status overlay snapshot. Keeping
/// these together makes the checkpoint domain explicit without changing the
/// existing store semantics.
#[derive(Debug, Default)]
pub(crate) struct CheckpointUiState {
    store: CheckpointStore,
    pub(crate) overlay: Option<UiStateCheckpoint>,
}

impl CheckpointUiState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn should_save(&self, candidate: &UiStateCheckpoint, now: Instant) -> bool {
        self.store.should_save(candidate, now)
    }

    pub(crate) fn record_saved(&mut self, candidate: &UiStateCheckpoint, now: Instant) {
        self.store.record_saved(candidate, now);
    }

    pub(crate) fn has_saved(&self) -> bool {
        self.store.has_saved()
    }
}

/// Paint the Session Auto-Save Checkpoints status overlay (§12.9.5) as a
/// centered modal: a header naming the session whose checkpoint this is, a
/// read-only list of the logical fields the checkpoint would restore (scroll
/// anchor, focused entry, search, minimap pane), a `[restore]` affordance, and a
/// footer with the verb legend. The `[restore]` line registers a
/// [`interaction::ChromeKey::CheckpointRestore`] click target so a click restores
/// it (the mouse twin of `r`). Reads only the captured checkpoint snapshot, so
/// painting is constant-time and does no transcript walk.
pub(crate) fn render_surface(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &crate::TuiApp,
    checkpoint: &UiStateCheckpoint,
) {
    let title = Line::from(vec![
        Span::styled(
            " Session checkpoint ",
            Style::default()
                .fg(crate::render::theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "\u{2014} auto-saved UI state ",
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ]);
    let inner = modal::surface(frame, area, 64, 14, title, app.glyph_mode);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let has_saved = !checkpoint.session_id.is_empty();

    let header = if has_saved {
        Line::from(vec![
            Span::styled(
                "saved for session ",
                Style::default().fg(crate::render::theme::quiet()),
            ),
            Span::styled(
                checkpoint.session_id.clone(),
                Style::default().fg(crate::render::theme::secondary()),
            ),
        ])
    } else {
        Line::from(Span::styled(
            "no checkpoint saved yet \u{2014} one is auto-saved as you work",
            Style::default().fg(crate::render::theme::quiet()),
        ))
    };
    frame.render_widget(Paragraph::new(header), Rect { height: 1, ..inner });

    let scroll_value = if checkpoint.following_tail {
        "following tail".to_string()
    } else {
        format!("{} line(s) up", checkpoint.scroll_from_bottom)
    };
    let selected_value = match checkpoint.selected_entry {
        Some(index) => format!("entry #{index}"),
        None => "\u{2014}".to_string(),
    };
    let search_value = checkpoint
        .search_query
        .clone()
        .unwrap_or_else(|| "\u{2014}".to_string());
    let minimap_value = if checkpoint.show_minimap {
        "shown"
    } else {
        "hidden"
    };
    let rows: [(&str, String); 4] = [
        ("Scroll", scroll_value),
        ("Focused entry", selected_value),
        ("Search", search_value),
        ("Minimap pane", minimap_value.to_string()),
    ];
    let rows_top = inner.y.saturating_add(2);
    let value_col = inner.x.saturating_add(18).min(inner.x + inner.width);
    let rows_bottom = inner.y.saturating_add(inner.height).saturating_sub(2);
    for (offset, (label, value)) in rows.iter().enumerate() {
        let row_y = rows_top + offset as u16;
        if row_y >= rows_bottom {
            break;
        }
        let label_rect = Rect {
            x: inner.x,
            y: row_y,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                *label,
                Style::default().fg(crate::render::theme::foreground()),
            ))),
            label_rect,
        );
        if value_col < inner.x.saturating_add(inner.width) {
            let value_rect = Rect {
                x: value_col,
                y: row_y,
                width: inner
                    .x
                    .saturating_add(inner.width)
                    .saturating_sub(value_col),
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    value.clone(),
                    Style::default().fg(crate::render::theme::secondary()),
                ))),
                value_rect,
            );
        }
    }

    let restore_y = inner.y.saturating_add(inner.height).saturating_sub(2);
    if has_saved && restore_y >= rows_top {
        let restore_rect = Rect {
            x: inner.x,
            y: restore_y,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "[restore]",
                Style::default()
                    .fg(crate::render::theme::accent())
                    .add_modifier(Modifier::BOLD),
            ))),
            restore_rect,
        );
        let target_rect = Rect {
            x: inner.x,
            y: restore_y,
            width: 9u16.min(inner.width),
            height: 1,
        };
        app.register_click(
            target_rect,
            interaction::TargetKey::Chrome(interaction::ChromeKey::CheckpointRestore),
            interaction::Action::CheckpointRestore,
        );
    }

    let footer_y = inner.y.saturating_add(inner.height).saturating_sub(1);
    if footer_y >= rows_top {
        let footer_rect = Rect {
            x: inner.x,
            y: footer_y,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "r restore \u{00b7} x forget \u{00b7} Esc close",
                Style::default()
                    .fg(crate::render::theme::secondary())
                    .add_modifier(Modifier::BOLD),
            ))),
            footer_rect,
        );
    }
}

/// Resolve the directory the per-session checkpoints are stored under. Honours
/// the [`CHECKPOINT_DIR_ENV`] override (tests / eval harness), else the
/// production [`squeezy_core::default_projects_dir`] (the platform state/config
/// location: `~/.squeezy/projects`, `$XDG_CONFIG_HOME/squeezy/...`,
/// `%APPDATA%\squeezy\...`).
fn checkpoint_root_dir() -> PathBuf {
    if let Some(custom) = std::env::var_os(CHECKPOINT_DIR_ENV) {
        return PathBuf::from(custom);
    }
    squeezy_core::default_projects_dir()
}

/// Sanitise a session id into a single safe path component. Session ids are
/// normally simple slugs, but a defensive map keeps a stray `/` or `..` from
/// escaping the store directory; any non-alphanumeric/`-`/`_` char becomes `_`.
fn session_file_stem(session_id: &str) -> String {
    let mapped: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if mapped.is_empty() {
        "unassigned".to_string()
    } else {
        mapped
    }
}

/// Resolve the on-disk path of a session's checkpoint: `<projects
/// dir>/_ui_checkpoints/<sanitised session id>.toml`. Stored OUTSIDE any repo
/// under the existing Squeezy projects state hierarchy so a save never dirties
/// the worktree.
pub(crate) fn checkpoint_path(session_id: &str) -> PathBuf {
    checkpoint_root_dir()
        .join("_ui_checkpoints")
        .join(format!("{}.toml", session_file_stem(session_id)))
}

/// Load a session's persisted checkpoint, or `None` when no file exists, the
/// file is unreadable/unparseable, it carries a newer schema version than this
/// build understands, or its embedded session id does not match `session_id`.
/// Never errors: a missing or malformed checkpoint is treated as "nothing to
/// restore" so a bad file can never block launch.
pub(crate) fn load(session_id: &str) -> Option<UiStateCheckpoint> {
    let path = checkpoint_path(session_id);
    // The common "no checkpoint exists" case stays silent — a fresh session has
    // nothing to restore. Every *other* failure (a file that exists but is
    // unreadable, malformed, newer-schema, or for a different session) is logged
    // once at debug so a silently-discarded UI state is at least traceable.
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::debug!(%err, "ui checkpoint unreadable; using defaults");
            return None;
        }
    };
    let checkpoint = match toml::from_str::<UiStateCheckpoint>(&text) {
        Ok(checkpoint) => checkpoint,
        Err(err) => {
            tracing::debug!(%err, "ui checkpoint corrupt; using defaults");
            return None;
        }
    };
    if checkpoint.version > CHECKPOINT_SCHEMA_VERSION {
        tracing::debug!(
            found = checkpoint.version,
            current = CHECKPOINT_SCHEMA_VERSION,
            "ui checkpoint ignored: newer schema; using defaults"
        );
        return None;
    }
    if !checkpoint.matches_session(session_id) {
        // Don't log the id itself — a session id is treated as sensitive; the
        // message alone is enough to trace a discarded checkpoint.
        tracing::debug!("ui checkpoint ignored: session id mismatch");
        return None;
    }
    Some(checkpoint)
}

/// Persist `checkpoint` for its session, creating the parent directory as
/// needed and writing atomically (`<file>.tmp` → rename) so a crash mid-write
/// can never leave a half-written checkpoint that fails to parse on the next
/// launch. Returns the path written on success. The file lives under the Squeezy
/// projects state dir, never inside the repo, so saving never dirties the
/// worktree.
pub(crate) fn save(checkpoint: &UiStateCheckpoint) -> std::io::Result<PathBuf> {
    let path = checkpoint_path(&checkpoint.session_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(checkpoint)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Forget a session's checkpoint by removing its on-disk file. A missing file is
/// a success (the session already has no checkpoint), so a double clear is
/// harmless.
pub(crate) fn clear(session_id: &str) -> std::io::Result<()> {
    let path = checkpoint_path(session_id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// A single process-global lock that serialises EVERY test mutation of the
/// process-global [`CHECKPOINT_DIR_ENV`] override — both the unit tests in
/// `session_checkpoint_tests.rs` AND the end-to-end tests in `lib_tests.rs`.
/// They share this one mutex (rather than each owning a private one) so a unit
/// test never clobbers the env dir out from under a concurrently-running
/// integration test on the test runner's threads. Test-only; no runtime weight.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
#[path = "session_checkpoint_tests.rs"]
mod tests;
