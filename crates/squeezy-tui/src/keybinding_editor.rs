//! Keybinding Editor UI (§12.7.1): an interactive overlay to inspect and rebind
//! every rebindable [`keymap::Action`] from inside the TUI.
//!
//! **Sourced from the registry, never a second table.** The editor's row list is
//! built straight from [`keymap::Action::ALL`] and the live
//! [`keymap::KeymapResolver`], so it lists *exactly* the actions the rest of the
//! TUI dispatches — a new `Action` variant appears here for free, with no parallel
//! list to keep in sync. Each row carries the action's slug, its currently
//! resolved binding, whether that binding differs from the compiled-in default,
//! and the action's `terminal_compat_note`, so the user sees the same diagnostics
//! `/keymap` prints, now editable.
//!
//! **Two modes, one state machine.** The resting mode is [`Mode::Browse`]: the
//! cursor walks the rows (↑↓/kj, Home/End/PageUp/PageDown) and the user reads the
//! current map. Pressing the capture verb (Enter, or a click on a row's "rebind"
//! affordance) flips the selected row into [`Mode::Capture`], where the *next*
//! key event the overlay sees is interpreted as the new chord rather than as a
//! navigation key. The captured chord is held as a [`PendingChord`] alongside its
//! conflict verdict so the surface can warn before the user commits.
//!
//! **Conflict + guardrails.** A captured chord is classified by
//! [`capture_outcome`]: it is rejected outright if it lands on a reserved
//! recovery binding (`Ctrl+C` / `Esc` / `Ctrl+D` — the TUI's emergency exits,
//! shared with [`crate::keymap_config::reserved_label`]), warned about if it
//! collides with another action's current binding (the user may still commit,
//! shadowing the other action — exactly what `/keymap` reports as a collision),
//! and accepted cleanly otherwise. The capture never mutates the resolver on its
//! own; the caller applies the committed override and persists it.
//!
//! **Pure model.** Like the other §12 leaf modules (`first_run_hints`,
//! `macros`), this file owns only the editor's state machine and the chord-
//! capture/conflict logic; it does NOT depend on `lib.rs`'s `TuiApp`, render a
//! frame, touch the filesystem, or rebuild the resolver. The caller (`lib.rs`)
//! opens it with a snapshot of the resolver, pumps key/mouse events into it, and
//! — when a chord is committed — reads the resulting override out and persists it.
//!
//! **Zero idle cost.** The overlay does not exist until opened
//! (`Option<KeybindingEditorState>` resting at `None`), so a session that never
//! opens it allocates nothing, paints nothing, and schedules no redraw.

use crossterm::event::{KeyCode, KeyModifiers};

use crate::keymap::{Action, KeyBinding};

/// One row in the editor: an action, its current binding, and the
/// already-computed diagnostics so the render path stays a pure projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EditorRow {
    /// The action this row rebinds.
    pub(crate) action: Action,
    /// The binding the live resolver currently maps the action to.
    pub(crate) binding: KeyBinding,
    /// True when `binding` differs from `action.default_binding()` — i.e. the
    /// user (or a config file) has already overridden it.
    pub(crate) is_override: bool,
}

/// The interaction mode the open editor is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Walking the row list, reading the current map.
    Browse,
    /// The next key event is captured as a new chord for the selected row.
    Capture,
}

/// The verdict for a captured chord, computed by [`capture_outcome`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CaptureOutcome {
    /// The chord is free — no other action binds it and it is not reserved.
    Free,
    /// The chord already drives another action (by slug). The user may still
    /// commit, shadowing it — the surface warns first.
    Conflict { with: &'static str },
    /// The chord is a reserved recovery binding (`Ctrl+C` / `Esc` / `Ctrl+D`)
    /// and may not be bound; the capture is refused with this label.
    Reserved { label: &'static str },
}

impl CaptureOutcome {
    /// True when the chord may be committed (free or a warnable conflict, but
    /// never a reserved recovery key).
    pub(crate) fn is_committable(&self) -> bool {
        !matches!(self, CaptureOutcome::Reserved { .. })
    }
}

/// A chord the user captured in [`Mode::Capture`], paired with its verdict so the
/// surface can render the warning and the key handler can refuse a reserved key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingChord {
    /// The normalised binding the captured key event resolves to.
    pub(crate) binding: KeyBinding,
    /// Whether the chord is free / conflicting / reserved.
    pub(crate) outcome: CaptureOutcome,
}

/// Reserved recovery bindings the editor refuses to capture, mirroring
/// `keymap_config::RESERVED_BINDINGS`. These are the TUI's only guaranteed exits
/// (`Ctrl+C` cancel/quit, `Esc` dismiss, `Ctrl+D` composer EOF); letting the user
/// rebind some action onto one of them would strand them with no way out, so the
/// editor protects them exactly as the file loader does.
const RESERVED: &[(&str, KeyCode, KeyModifiers)] = &[
    ("Ctrl+C", KeyCode::Char('c'), KeyModifiers::CONTROL),
    ("Esc", KeyCode::Esc, KeyModifiers::NONE),
    ("Ctrl+D", KeyCode::Char('d'), KeyModifiers::CONTROL),
];

/// Return the reserved-binding label `binding` matches, or `None`. Character
/// codes compare case-insensitively so a `Ctrl+c` capture is caught the same as
/// `Ctrl+C`.
pub(crate) fn reserved_label(binding: &KeyBinding) -> Option<&'static str> {
    for (label, code, mods) in RESERVED {
        if binding.modifiers != *mods {
            continue;
        }
        let matches = match (binding.code, *code) {
            (KeyCode::Char(a), KeyCode::Char(b)) => a.eq_ignore_ascii_case(&b),
            (a, b) => a == b,
        };
        if matches {
            return Some(label);
        }
    }
    None
}

/// Classify a captured `binding` for the row currently being edited (`editing`).
/// A chord that lands on a reserved recovery key is [`CaptureOutcome::Reserved`];
/// one that another *different* action already binds is
/// [`CaptureOutcome::Conflict`] (naming that action's slug); a chord equal to the
/// editing row's own current binding, or bound by nothing else, is
/// [`CaptureOutcome::Free`]. The conflict scan walks the supplied `rows`, so it
/// reflects the live map the editor was opened with (including earlier edits the
/// caller folded back in).
pub(crate) fn capture_outcome(
    rows: &[EditorRow],
    editing: Action,
    binding: KeyBinding,
) -> CaptureOutcome {
    if let Some(label) = reserved_label(&binding) {
        return CaptureOutcome::Reserved { label };
    }
    for row in rows {
        if row.action == editing {
            continue;
        }
        if row.binding == binding {
            return CaptureOutcome::Conflict {
                with: row.action.slug(),
            };
        }
    }
    CaptureOutcome::Free
}

/// The interactive editor's full state while the overlay is open.
#[derive(Debug, Clone)]
pub(crate) struct KeybindingEditorState {
    /// One row per rebindable action, in `Action::ALL` order.
    rows: Vec<EditorRow>,
    /// The selected row index (always `< rows.len()` while `rows` is non-empty).
    selected: usize,
    /// Browse vs. capture.
    mode: Mode,
    /// The chord captured in the current capture session, if any has been
    /// pressed yet (the surface shows it with its verdict before commit).
    pending: Option<PendingChord>,
}

impl KeybindingEditorState {
    /// Open the editor over the current map. `rows` is built by the caller from
    /// `Action::ALL` and the live resolver so the registry stays the single
    /// source of truth. The cursor starts at the top in [`Mode::Browse`].
    pub(crate) fn new(rows: Vec<EditorRow>) -> Self {
        Self {
            rows,
            selected: 0,
            mode: Mode::Browse,
            pending: None,
        }
    }

    pub(crate) fn rows(&self) -> &[EditorRow] {
        &self.rows
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(crate) fn selected_index(&self) -> usize {
        self.selected
    }

    pub(crate) fn selected_row(&self) -> Option<&EditorRow> {
        self.rows.get(self.selected)
    }

    pub(crate) fn is_capturing(&self) -> bool {
        self.mode == Mode::Capture
    }

    pub(crate) fn pending(&self) -> Option<&PendingChord> {
        self.pending.as_ref()
    }

    /// Move the cursor up one row (no wrap, clamped at the top).
    pub(crate) fn select_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Move the cursor down one row (no wrap, clamped at the bottom).
    pub(crate) fn select_down(&mut self) {
        if self.selected + 1 < self.rows.len() {
            self.selected += 1;
        }
    }

    /// Move the cursor up by `page` rows (clamped at the top).
    pub(crate) fn page_up(&mut self, page: usize) {
        self.selected = self.selected.saturating_sub(page.max(1));
    }

    /// Move the cursor down by `page` rows (clamped at the bottom).
    pub(crate) fn page_down(&mut self, page: usize) {
        let last = self.rows.len().saturating_sub(1);
        self.selected = (self.selected + page.max(1)).min(last);
    }

    /// Jump the cursor to the first row.
    pub(crate) fn select_first(&mut self) {
        self.selected = 0;
    }

    /// Jump the cursor to the last row.
    pub(crate) fn select_last(&mut self) {
        self.selected = self.rows.len().saturating_sub(1);
    }

    /// Move the cursor onto `index` (clamped), used by a mouse click on a row.
    /// Returns `true` when the selection actually changed. A click while
    /// capturing is ignored (the capture session owns the keyboard); the caller
    /// gates on [`is_capturing`](Self::is_capturing) before routing a click here.
    pub(crate) fn select_index(&mut self, index: usize) -> bool {
        if self.rows.is_empty() {
            return false;
        }
        let clamped = index.min(self.rows.len() - 1);
        if clamped == self.selected {
            return false;
        }
        self.selected = clamped;
        true
    }

    /// Enter capture mode for the selected row. A no-op (returns `false`) when
    /// there are no rows or the editor is already capturing. Clears any stale
    /// pending chord so the surface starts from "press a key".
    pub(crate) fn begin_capture(&mut self) -> bool {
        if self.rows.is_empty() || self.mode == Mode::Capture {
            return false;
        }
        self.mode = Mode::Capture;
        self.pending = None;
        true
    }

    /// Leave capture mode without committing, dropping the pending chord. Returns
    /// `true` when it actually left capture mode (so the caller can distinguish a
    /// capture-cancel Esc from a close-the-editor Esc).
    pub(crate) fn cancel_capture(&mut self) -> bool {
        if self.mode != Mode::Capture {
            return false;
        }
        self.mode = Mode::Browse;
        self.pending = None;
        true
    }

    /// Record a captured chord for the selected row, computing its verdict. The
    /// chord stays pending — it is NOT committed — so the surface can show the
    /// conflict/reserved warning and the user confirms with a second press.
    /// Returns the verdict, or `None` when not capturing / no row is selected.
    pub(crate) fn capture(&mut self, binding: KeyBinding) -> Option<CaptureOutcome> {
        if self.mode != Mode::Capture {
            return None;
        }
        let editing = self.selected_row()?.action;
        let outcome = capture_outcome(&self.rows, editing, binding);
        self.pending = Some(PendingChord {
            binding,
            outcome: outcome.clone(),
        });
        Some(outcome)
    }

    /// Commit the pending chord onto the selected row, returning the
    /// `(action, binding)` the caller should persist + apply. Refuses (returns
    /// `None`) when there is no committable pending chord (none captured yet, or
    /// the captured chord is reserved). On success the row's binding is updated
    /// in place (so the in-overlay list reflects the change immediately, and a
    /// later conflict scan sees it), `is_override` is recomputed, and the editor
    /// returns to [`Mode::Browse`].
    pub(crate) fn commit(&mut self) -> Option<(Action, KeyBinding)> {
        if self.mode != Mode::Capture {
            return None;
        }
        let pending = self.pending.take()?;
        if !pending.outcome.is_committable() {
            // Keep the reserved chord visible so the user sees why nothing
            // happened; stay in capture mode awaiting a different key.
            self.pending = Some(pending);
            return None;
        }
        let index = self.selected;
        let row = self.rows.get_mut(index)?;
        row.binding = pending.binding;
        row.is_override = pending.binding != row.action.default_binding();
        let result = (row.action, pending.binding);
        self.mode = Mode::Browse;
        Some(result)
    }

    /// Reset the selected row back to its compiled-in default, returning the
    /// `(action, default_binding)` the caller should persist (by dropping the
    /// override). A no-op (returns `None`) when the row already holds the
    /// default. Updates the in-overlay row in place so the list reflects the
    /// reset immediately.
    pub(crate) fn reset_selected(&mut self) -> Option<(Action, KeyBinding)> {
        let index = self.selected;
        let row = self.rows.get_mut(index)?;
        let default = row.action.default_binding();
        if row.binding == default {
            return None;
        }
        row.binding = default;
        row.is_override = false;
        Some((row.action, default))
    }
}

#[cfg(test)]
#[path = "keybinding_editor_tests.rs"]
mod tests;
