//! Frame-local hit-test registry + focus model + gesture recognizer — the
//! Phase 7B direct-manipulation substrate.
//!
//! This module formalizes the ad-hoc click plumbing that previously lived as a
//! bare `Vec<Clickable>` + a footer-only `ClickAction` enum in `lib.rs` and a
//! separate row-local `ClickTarget`/`ClickAction` in `transcript_surface.rs`.
//! It unifies all of that into one id-anchored vocabulary so a clickable target
//! is *keyed by identity* (an `EntryId`, a `RowId`, a queue-item id, or a small
//! set of chrome keys), not by a remembered cursor coordinate. Rects are
//! recomputed every frame from current geometry; the key is the stable handle,
//! so a target "moves" on resize without the hit-test ever consulting a stale
//! position.
//!
//! It is a peer leaf module beside `selection`/`scroll`: it depends only on the
//! id newtypes in [`crate::transcript_surface`], on [`crate::keymap`], and on
//! `ratatui::layout::Rect`. It does NOT depend back on `lib.rs`'s `TuiApp`,
//! mirroring the discipline `transcript_surface.rs` already keeps, so every
//! piece here (hit-test, focus resolver, gesture transitions) is a pure
//! function over model state and is unit-testable without a terminal.

use std::time::Instant;

use ratatui::layout::Rect;

use crate::transcript_surface::{EntryId, RowId};

// ===========================================================================
// Target keys + actions — the unified hit-test vocabulary
// ===========================================================================

/// The *stable* identity of a clickable region. This is the mechanism that
/// makes targets survive reflow/resize: a target is addressed by id, never by
/// screen coordinates. The same key re-registers at a fresh `Rect` each frame.
///
/// Carrying the key alongside the action (see [`Registry::hit_test`]) lets a
/// caller tell *which* card/row was hit even when two cards share the same
/// action variant.
///
/// `Entry` and `Chrome(QueueStrip)` are registered today (card headers/carets
/// and the queue strip). `RowSpan` (sub-row code-block copy), `QueueItem`
/// (delete/reorder), and the `JumpToLatest`/`ScrollbarGutter` chrome keys are
/// the substrate vocabulary their affordances register in later phases; the
/// hit-test handles them uniformly already and the tests exercise them.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TargetKey {
    /// A whole transcript entry: card header, disclosure caret, per-entry copy.
    /// Derived from [`crate::transcript_surface::TranscriptRow::entry_id`], so
    /// it survives coalescing/reflow/resize.
    Entry(EntryId),
    /// A sub-row affordance: a specific row plus an in-row char span (e.g. a
    /// code-block copy button). Derived from `RowId` + a `ClickTarget`'s
    /// `text_range`.
    RowSpan(RowId, RowSpan),
    /// A prompt-queue item, addressed by its stable per-item id, NOT its Vec
    /// index — so a reorder/delete mid-gesture never shifts the hit target.
    QueueItem(u64),
    /// A chrome affordance that carries no entry/row id.
    Chrome(ChromeKey),
}

/// Half-open char-offset span within a row's plain text. A plain `Copy`
/// newtype so a [`TargetKey`] stays `Copy`/`Hash` (a bare `Range<usize>` is
/// neither `Copy` nor `Hash`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RowSpan {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl RowSpan {
    /// Constructed by the code-block-copy `RowSpan` affordance (and the tests);
    /// part of the substrate's sub-row addressing vocabulary.
    #[allow(dead_code)]
    pub(crate) fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

/// Chrome affordances with no entry/row identity of their own. `QueueStrip` is
/// registered today; `JumpToLatest`/`ScrollbarGutter` register with their
/// affordances in later phases.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ChromeKey {
    /// The prompt-queue indicator strip in the footer.
    QueueStrip,
    /// The jump-to-latest affordance.
    JumpToLatest,
    /// The main-view scrollbar gutter.
    ScrollbarGutter,
}

/// What a click on a registered target does. This unifies the two action
/// enums that previously coexisted (`lib.rs`'s footer `ClickAction` and
/// `transcript_surface.rs`'s row-local `ClickAction`). Each variant maps 1:1
/// to an existing or new handler, dispatched in `lib.rs::dispatch_click_action`
/// — the same handlers the keyboard path calls, so keyboard/mouse parity holds
/// by construction.
///
/// `ToggleQueueOverlay`, `ToggleEntryCollapsed`, `FocusEntry`, and `ExpandEntry`
/// are wired to live affordances today. `OpenEntryInDetail` (mouse twin of the
/// `Ctrl+Enter` keyboard verb, which goes straight through
/// `open_focused_entry_in_detail`), the queue delete/reorder actions, and the
/// jump/scrollbar actions complete the unified vocabulary and dispatch their
/// handlers as their registering affordances land.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// Open / close the prompt-queue reorder overlay. (Port of the old footer
    /// `ClickAction::ToggleQueueOverlay`.)
    ToggleQueueOverlay,
    /// Toggle the given entry's collapsed/expanded state. (Port of the old
    /// row-local `ClickAction::ToggleEntryCollapsed`.) Fed by a caret click.
    ToggleEntryCollapsed(EntryId),
    /// Make the given entry the focused entry. Fed by a card-header click.
    FocusEntry(EntryId),
    /// Expand the given entry *only if it is currently collapsed* (idempotent
    /// expand). Fed by a double-click on a collapsed card.
    ExpandEntry(EntryId),
    /// Open the given entry in the Ctrl+T detail overlay.
    OpenEntryInDetail(EntryId),
    /// Delete the given queue item (by stable item id).
    QueueDelete(u64),
    /// Begin a reorder drag of the given queue item (by stable item id).
    QueueReorderBegin(u64),
    /// Jump the transcript to the latest (tail) row.
    JumpToLatest,
    /// Jump the scrollbar thumb to the clicked gutter row.
    ScrollbarJump,
}

// ===========================================================================
// Frame-local hit-test registry
// ===========================================================================

/// One registered clickable region for the frame currently being drawn.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Hit {
    pub(crate) rect: Rect,
    pub(crate) key: TargetKey,
    pub(crate) action: Action,
}

/// The frame-local hit-test registry. Owned by `TuiApp` behind a `RefCell`
/// (render fns hold only `&TuiApp`), cleared at the top of every draw via
/// [`Registry::begin_frame`] and repopulated by [`Registry::register`].
///
/// Replaces the bare `Vec<Clickable>` + `register_click`/`click_target_at`
/// trio. The hit-test iterates in reverse so a later-drawn overlay wins over an
/// earlier widget at the same cell, exactly as the old `click_target_at` did.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    hits: Vec<Hit>,
}

impl Registry {
    pub(crate) fn new() -> Self {
        Self { hits: Vec::new() }
    }

    /// Clear the registry at the start of a frame.
    pub(crate) fn begin_frame(&mut self) {
        self.hits.clear();
    }

    /// Record a clickable region for the current frame.
    pub(crate) fn register(&mut self, rect: Rect, key: TargetKey, action: Action) {
        self.hits.push(Hit { rect, key, action });
    }

    /// Topmost target containing `(column, row)`, if any. Iterates in reverse
    /// so later-registered (later-drawn) targets take precedence — the same
    /// "topmost wins" semantics the old `click_target_at` had. Returns the key
    /// alongside the action so the caller knows *which* target was hit.
    pub(crate) fn hit_test(&self, column: u16, row: u16) -> Option<(TargetKey, Action)> {
        self.hits
            .iter()
            .rev()
            .find(|h| rect_contains(h.rect, column, row))
            .map(|h| (h.key, h.action))
    }

    /// Number of registered targets this frame (test/diagnostic aid).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.hits.len()
    }
}

/// Half-open containment: `column ∈ [x, x+width)` and `row ∈ [y, y+height)`.
/// Matches the old `click_target_at` bounds check exactly.
fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

// ===========================================================================
// Focus model
// ===========================================================================

/// The focused-entry cursor, expressed as an [`EntryId`]-or-none rather than a
/// fragile transcript *index*. The id is the same stable handle the render
/// cache and row model already key on, so it survives entries being
/// pruned/coalesced — whereas an index would drift.
///
/// Callees that take an index keep working via [`Focus::resolve_index`], which
/// maps the focused id back to a live transcript index on demand (linear find
/// by `entry.id`, the same pattern `assistant_entry_ids` /
/// `build_transcript_rows_uncached` already use).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Focus {
    entry: Option<EntryId>,
}

impl Focus {
    pub(crate) fn new() -> Self {
        Self { entry: None }
    }

    /// The currently focused entry id, if any. Read by the recognizer/focus
    /// tests and by dispatch paths that want the id without re-resolving an
    /// index.
    #[allow(dead_code)]
    pub(crate) fn focused(self) -> Option<EntryId> {
        self.entry
    }

    /// Set the focus directly to a given entry (mouse header-click, pin picker).
    pub(crate) fn set(&mut self, entry: EntryId) {
        self.entry = Some(entry);
    }

    /// Clear the focus.
    pub(crate) fn clear(&mut self) {
        self.entry = None;
    }

    /// Adopt focus from a raw transcript index against the given id order.
    /// Keeps the id-based focus in sync when a legacy index-based path
    /// (`selected_entry`) is the source of truth.
    pub(crate) fn set_from_index(&mut self, index: Option<usize>, ids: &[u64]) {
        self.entry = index.and_then(|i| ids.get(i)).map(|id| EntryId(*id));
    }

    /// Resolve the focused id back to a live index in `ids` (the transcript's
    /// entry-id order). `None` when nothing is focused or the id is no longer
    /// present (entry pruned). This is the shim every index-taking callee uses.
    pub(crate) fn resolve_index(self, ids: &[u64]) -> Option<usize> {
        let EntryId(want) = self.entry?;
        ids.iter().position(|id| *id == want)
    }

    /// Step the focus to the previous entry in `ids` order. Wraps to the last
    /// entry when nothing is focused yet (or the focused id was pruned),
    /// mirroring `select_previous_transcript_entry`. No-op on an empty order.
    /// Returns the resulting focused id, if any.
    pub(crate) fn focus_prev(&mut self, ids: &[u64]) -> Option<EntryId> {
        if ids.is_empty() {
            self.entry = None;
            return None;
        }
        let next = match self.resolve_index(ids) {
            Some(i) => i.saturating_sub(1),
            None => ids.len() - 1,
        };
        self.entry = Some(EntryId(ids[next]));
        self.entry
    }

    /// Step the focus to the next entry in `ids` order. Wraps to the first
    /// entry when nothing is focused yet (or the focused id was pruned),
    /// mirroring `select_next_transcript_entry`. No-op on an empty order.
    /// Returns the resulting focused id, if any.
    pub(crate) fn focus_next(&mut self, ids: &[u64]) -> Option<EntryId> {
        if ids.is_empty() {
            self.entry = None;
            return None;
        }
        let next = match self.resolve_index(ids) {
            Some(i) => (i + 1).min(ids.len() - 1),
            None => 0,
        };
        self.entry = Some(EntryId(ids[next]));
        self.entry
    }
}

// ===========================================================================
// Gesture recognizer
// ===========================================================================

/// A second/third press on the *same target key* within this window is treated
/// as a double/triple click. Promoted from `lib.rs`'s `MULTI_CLICK_MS`.
pub(crate) const MULTI_CLICK_MS: u128 = 400;

/// A hovered target must stay hovered (same key) for at least this long before
/// hover affordances reveal — debounces flicker as the pointer sweeps across
/// targets. Only relevant when terminal mouse capture is on; otherwise no
/// Move/Drag events arrive and the recognizer stays inert.
pub(crate) const HOVER_INTENT_MS: u128 = 150;

/// Raw mouse-button phase fed to the recognizer, distilled from crossterm's
/// `MouseEventKind` so this module needn't depend on crossterm directly. The
/// caller (`handle_mouse`) translates `Down/Drag/Up/Moved` into these.
///
/// `Press` drives the card-affordance click/double-click path today. `Drag`,
/// `Release`, and `Move` are the substrate the queue-reorder drag and hover
/// affordances feed (they recognize the same way); they are exercised by the
/// recognizer's unit tests and land in `handle_mouse` with those affordances.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Left button pressed at the cell.
    Press,
    /// Pointer moved with the left button held.
    Drag,
    /// Left button released.
    Release,
    /// Pointer moved with no button held (only delivered while capture is on).
    Move,
}

/// A semantic gesture produced by the recognizer from the raw button stream
/// plus the registry hit-test result. The dispatch layer turns each into the
/// matching handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gesture {
    /// A single click landed on `target` (or `None` for empty space).
    Click {
        target: Option<TargetKey>,
        action: Option<Action>,
    },
    /// A second click on the same target within [`MULTI_CLICK_MS`].
    DoubleClick {
        target: Option<TargetKey>,
        action: Option<Action>,
    },
    /// A third click on the same target within [`MULTI_CLICK_MS`].
    TripleClick {
        target: Option<TargetKey>,
        action: Option<Action>,
    },
    /// A drag began on `target`.
    DragStart { target: Option<TargetKey> },
    /// A drag is in progress; `target` is the currently hovered key (the live
    /// insertion marker is computed from this each event, never from pixels).
    DragExtend { target: Option<TargetKey> },
    /// A drag ended; `target` is the drop key.
    DragEnd { target: Option<TargetKey> },
    /// The pointer hovered onto `target` and the hover-intent delay elapsed on
    /// that same key.
    HoverEnter { target: TargetKey },
    /// The pointer left the previously hovered target.
    HoverLeave,
    /// The event produced no semantic gesture (e.g. a `Move` whose intent
    /// delay has not yet elapsed, or a release with no in-flight drag).
    None,
}

/// Multiplicity state of the most recent press, keyed on the **target key**
/// (not the screen cell). Keying on the key is the correctness fix the design
/// calls out: a double-click that lands one cell off, or after a reflow, must
/// still count as a double — comparing screen cells (as the old `last_click`
/// did) would miscount it as two singles.
#[derive(Debug, Clone, Copy)]
struct PressState {
    at: Instant,
    target: Option<TargetKey>,
    multiplicity: u8,
}

/// In-flight drag state — all model-space, never live cursor coordinates as
/// authority. It stores *what* is being dragged (`origin` key) and the current
/// hovered key (`current`); the live insertion marker is re-derived from
/// `current` each Drag, so a resize mid-drag re-resolves from ids and never
/// desyncs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DragState {
    /// The key the drag started on.
    pub(crate) origin: Option<TargetKey>,
    /// The key the pointer is currently over (the insertion anchor).
    pub(crate) current: Option<TargetKey>,
}

/// Hover-intent state: which key is hovered and when it was first hovered.
#[derive(Debug, Clone, Copy)]
struct HoverState {
    target: TargetKey,
    since: Instant,
    /// Whether [`HOVER_INTENT_MS`] has already elapsed and the enter gesture
    /// was emitted (so we don't re-emit every Move on the same key).
    armed: bool,
}

/// The gesture recognizer. Owned by `TuiApp`. Turns the raw `Press/Drag/
/// Release/Move` stream into semantic [`Gesture`]s. It holds only model-space
/// state: last-press timing/key/multiplicity, an optional drag, and an
/// optional hover-intent. It owns no live cursor coordinates as authority.
#[derive(Debug, Default)]
pub(crate) struct Recognizer {
    last_press: Option<PressState>,
    drag: Option<DragState>,
    hover: Option<HoverState>,
}

impl Recognizer {
    pub(crate) fn new() -> Self {
        Self {
            last_press: None,
            drag: None,
            hover: None,
        }
    }

    /// The in-flight drag, if any. The dispatch layer reads this to render the
    /// live insertion marker. Consumed by the recognizer tests today; wired
    /// into `handle_mouse` with the queue-reorder drag affordance.
    #[allow(dead_code)]
    pub(crate) fn drag(&self) -> Option<DragState> {
        self.drag
    }

    /// True while a drag is in progress. (See [`Recognizer::drag`].)
    #[allow(dead_code)]
    pub(crate) fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Recognize a single mouse event. `hit` is the registry hit-test result
    /// for the event's cell (key + action), or `None` for empty space. `now`
    /// is injected so the multi-click / hover-intent timing is testable without
    /// a real clock.
    ///
    /// The press path keys multiplicity on the *target key*, gated by
    /// [`MULTI_CLICK_MS`]. Drag transitions store target keys, never pixels.
    /// Hover only arms after [`HOVER_INTENT_MS`] on the same key.
    pub(crate) fn recognize(
        &mut self,
        phase: Phase,
        hit: Option<(TargetKey, Action)>,
        now: Instant,
    ) -> Gesture {
        let target = hit.map(|(k, _)| k);
        let action = hit.map(|(_, a)| a);
        match phase {
            Phase::Press => self.on_press(target, action, now),
            Phase::Drag => self.on_drag(target),
            Phase::Release => self.on_release(target),
            Phase::Move => self.on_move(target, now),
        }
    }

    fn on_press(
        &mut self,
        target: Option<TargetKey>,
        action: Option<Action>,
        now: Instant,
    ) -> Gesture {
        // Multiplicity escalates only when the SAME target key is re-pressed
        // within the window. Keying on the id (not the cell) is what keeps a
        // double-click correct across a 1-cell jitter or a reflow.
        let multiplicity = match self.last_press {
            Some(prev)
                if prev.target == target
                    && now.duration_since(prev.at).as_millis() <= MULTI_CLICK_MS =>
            {
                (prev.multiplicity + 1).min(3)
            }
            _ => 1,
        };
        self.last_press = Some(PressState {
            at: now,
            target,
            multiplicity,
        });
        // A fresh press also begins a potential drag (resolved on the first
        // Drag event). Hover intent is cleared while a button is down.
        self.drag = Some(DragState {
            origin: target,
            current: target,
        });
        self.hover = None;
        match multiplicity {
            2 => Gesture::DoubleClick { target, action },
            3 => Gesture::TripleClick { target, action },
            _ => Gesture::Click { target, action },
        }
    }

    fn on_drag(&mut self, target: Option<TargetKey>) -> Gesture {
        match self.drag.as_mut() {
            Some(drag) => {
                let was_origin = drag.current == drag.origin && drag.origin.is_some();
                drag.current = target;
                // First movement off the origin promotes the press into a drag
                // gesture; subsequent movements extend it. We surface
                // DragStart on the first Drag event regardless so the dispatch
                // layer can arm its model-order insertion tracking.
                if was_origin {
                    Gesture::DragStart {
                        target: drag.origin,
                    }
                } else {
                    Gesture::DragExtend { target }
                }
            }
            None => {
                // A Drag with no recorded press (capture toggled mid-gesture):
                // start tracking from here so we don't desync.
                self.drag = Some(DragState {
                    origin: target,
                    current: target,
                });
                Gesture::DragStart { target }
            }
        }
    }

    fn on_release(&mut self, target: Option<TargetKey>) -> Gesture {
        match self.drag.take() {
            // A drag that actually moved off its origin ends with a drop.
            Some(drag) if drag.current != drag.origin => Gesture::DragEnd { target },
            // A press→release with no movement is a plain click, already
            // emitted on the press; the release is a no-op here.
            Some(_) => Gesture::None,
            None => Gesture::None,
        }
    }

    fn on_move(&mut self, target: Option<TargetKey>, now: Instant) -> Gesture {
        match target {
            None => {
                // Pointer left every target.
                if self.hover.take().is_some_and(|h| h.armed) {
                    Gesture::HoverLeave
                } else {
                    self.hover = None;
                    Gesture::None
                }
            }
            Some(key) => match self.hover {
                // Same key, already armed: nothing new.
                Some(h) if h.target == key && h.armed => Gesture::None,
                // Same key, intent delay elapsed: arm and emit enter.
                Some(h)
                    if h.target == key
                        && now.duration_since(h.since).as_millis() >= HOVER_INTENT_MS =>
                {
                    self.hover = Some(HoverState {
                        target: key,
                        since: h.since,
                        armed: true,
                    });
                    Gesture::HoverEnter { target: key }
                }
                // Same key, still waiting out the delay.
                Some(h) if h.target == key => Gesture::None,
                // Moved onto a different key (or first hover): if the previous
                // one was armed, that's a leave; (re)start the intent clock.
                prev => {
                    let leaving = prev.is_some_and(|h| h.armed);
                    self.hover = Some(HoverState {
                        target: key,
                        since: now,
                        armed: false,
                    });
                    if leaving {
                        Gesture::HoverLeave
                    } else {
                        Gesture::None
                    }
                }
            },
        }
    }

    /// Reset all in-flight gesture state. Called when mouse capture turns off
    /// or the surface changes out from under an in-flight gesture. Wired into
    /// `handle_mouse`'s capture-toggle path with the drag/hover affordances.
    #[allow(dead_code)]
    pub(crate) fn reset(&mut self) {
        self.last_press = None;
        self.drag = None;
        self.hover = None;
    }
}

#[cfg(test)]
#[path = "interaction_tests.rs"]
mod tests;
