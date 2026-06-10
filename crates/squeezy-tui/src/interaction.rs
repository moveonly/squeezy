//! Frame-local hit-test registry + gesture recognizer — the
//! Phase 7B direct-manipulation substrate.
//!
//! This module formalizes the ad-hoc click plumbing that previously lived as a
//! bare `Vec<Clickable>` + a footer-only `ClickAction` enum in `lib.rs` (plus a
//! never-populated row-local `ClickTarget`/`ClickAction` in `transcript_surface`
//! that this module replaced and which has since been removed). It unifies that
//! into one id-anchored vocabulary so a clickable target
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
//! piece here (hit-test, gesture transitions) is a pure
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
/// `Entry`, `Chrome(QueueStrip)`, and `QueueItem` (delete/reorder) are
/// registered today (card headers/carets, the queue strip, and the per-item
/// overlay affordances). `RowSpan` (sub-row code-block copy) and the
/// `JumpToLatest`/`ScrollbarGutter` chrome keys are the substrate vocabulary
/// their affordances register in later phases; the hit-test handles them
/// uniformly already and the tests exercise them.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TargetKey {
    /// A whole transcript entry: card header, disclosure caret, per-entry copy.
    /// Derived from [`crate::transcript_surface::TranscriptRow::entry_id`], so
    /// it survives coalescing/reflow/resize.
    Entry(EntryId),
    /// A sub-row affordance: a specific row plus an in-row char span (e.g. a
    /// code-block copy button). Derived from a [`RowId`] plus the affordance's
    /// `copy_text` char range.
    RowSpan(RowId, RowSpan),
    /// A prompt-queue item, addressed by its stable per-item id, NOT its Vec
    /// index — so a reorder/delete mid-gesture never shifts the hit target.
    QueueItem(u64),
    /// A clipboard-history entry in the picker overlay (§12.6.1), addressed by
    /// its stable per-entry id (NOT its list index) so an eviction mid-gesture
    /// never shifts the hit target.
    ClipboardEntry(u64),
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
    /// The "Accept" button in the large-paste confirmation modal (§11G.6).
    PasteConfirm,
    /// The "Discard" button in the large-paste confirmation modal (§11G.6).
    PasteCancel,
    /// A row in the paste-transform menu (§12.6.2), keyed by its 0-based index
    /// in the offered-transform list so a click selects exactly that shape.
    PasteTransformItem(usize),
    /// A row in the Large Paste Staging overlay (§12.6.3), keyed by its 0-based
    /// index in the offered-action list so a click selects exactly that action.
    PasteStagingItem(usize),
    /// The "Re-copy" button in the clipboard-history picker (§12.6.1).
    ClipboardRecopy,
    /// The "Delete" button in the clipboard-history picker (§12.6.1).
    ClipboardDelete,
    /// The "Clear all" button in the clipboard-history picker (§12.6.1).
    ClipboardClear,
}

/// What a click on a registered target does. This unifies the two action
/// enums that previously coexisted (`lib.rs`'s footer `ClickAction` and
/// `transcript_surface.rs`'s row-local `ClickAction`). Each variant maps 1:1
/// to an existing or new handler, dispatched in `lib.rs::dispatch_click_action`
/// — the same handlers the keyboard path calls, so keyboard/mouse parity holds
/// by construction.
///
/// `ToggleQueueOverlay`, `ToggleEntryCollapsed`, `FocusEntry`, `ExpandEntry`,
/// the queue `QueueDelete` / `QueueReorderBegin` / `QueueUndo` / `QueueEdit`
/// actions, and `MinimapJump` are wired to live affordances today (real dispatch
/// arms + registered hit targets + keyboard parity). Only `OpenEntryInDetail` (no
/// mouse affordance registers it yet; the `Ctrl+Enter` keyboard verb goes
/// straight through `open_focused_entry_in_detail`) and the jump/scrollbar
/// actions remain substrate that dispatches its handlers as its registering
/// affordances land.
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
    /// The press on a queue-item row arms a drag; the live move + drop are
    /// driven from the gesture recognizer's `DragState` in `handle_mouse`.
    QueueReorderBegin(u64),
    /// Undo the most recent queue mutation (delete or reorder). The mouse
    /// twin of the keyboard undo verb; both pop one entry off the queue's
    /// bounded undo stack and reverse it exactly.
    QueueUndo,
    /// Open the given queue item (by stable item id) in the composer for editing
    /// (§11G.8). The mouse twin of the keyboard `Enter`/`e` edit verb; both pull
    /// the prompt's text into the composer and track its id so the next submit
    /// updates that item in place. Fed by a double-click on a queue-item row.
    QueueEdit(u64),
    /// Promote the given queue item (by stable item id) to the front so it runs
    /// next (§11G.9). The mouse twin of the keyboard `r` verb; both move the item
    /// to the front of the queue, then — when idle — arm the drain pump so it
    /// starts immediately, or — when a turn is running — let the drain-on-finish
    /// path run it next ahead of the rest of the queue.
    QueueRunNext(u64),
    /// Jump the transcript to the latest (tail) row.
    JumpToLatest,
    /// Jump the scrollbar thumb to the clicked gutter row.
    ScrollbarJump,
    /// Jump the transcript so the entry behind a minimap turn-rail cell sits at
    /// the top of the viewport. Keyed by the cell's [`EntryId`] so a resize
    /// re-registers the same target at a fresh rail cell.
    MinimapJump(EntryId),
    /// Confirm the pending large paste in the confirmation modal (§11G.6),
    /// inserting it into the composer. Mouse twin of the modal's Enter/`y` key.
    ConfirmPaste,
    /// Cancel the pending large paste in the confirmation modal (§11G.6),
    /// discarding it. Mouse twin of the modal's Esc/`n` key.
    CancelPaste,
    /// Select (move the cursor to) the given row in the paste-transform menu
    /// (§12.6.2) and apply it. Mouse twin of moving the cursor with ↑↓ and
    /// pressing Enter; a click both selects and applies the shape in one go.
    PasteTransformSelect(usize),
    /// Select (move the cursor to) the given row in the Large Paste Staging
    /// overlay (§12.6.3) and apply it. Mouse twin of moving the cursor with ↑↓
    /// and pressing Enter; a click both selects and applies the action in one go.
    PasteStagingSelect(usize),
    /// Select the given clipboard-history entry (by stable id) in the picker
    /// (§12.6.1). Mouse twin of the picker's Up/Down arrows. Fed by a single
    /// click on a history row.
    ClipboardSelect(u64),
    /// Re-copy the given clipboard-history entry (by stable id) back to the
    /// clipboard (§12.6.1). Mouse twin of the picker's Enter verb / the
    /// "Re-copy" button. Fed by a double-click on a history row.
    ClipboardRecopy(u64),
    /// Delete the given clipboard-history entry (by stable id) from the in-app
    /// history (§12.6.1). Mouse twin of the picker's `d` verb / the "Delete"
    /// button.
    ClipboardDelete(u64),
    /// Clear the entire in-app clipboard history (§12.6.1). Mouse twin of the
    /// picker's `c` verb / the "Clear all" button.
    ClipboardClear,
}

impl Action {
    /// One representative of every [`Action`] variant, in a stable order. The
    /// payload-carrying variants use a sentinel id (the variant identity is what
    /// the audit cares about, not the specific target). The Accessibility
    /// Quality Gate (§12.10.5) sweeps this to prove every mouse affordance has a
    /// keyboard equivalent; any new variant must be added here or the gate's
    /// exhaustiveness assertion fails.
    ///
    /// `cfg(test)`-only: the only consumer is the gate, which is itself
    /// test-gated, so this carries no runtime weight on any platform.
    #[cfg(test)]
    pub(crate) const AUDIT_ALL: &'static [Action] = &[
        Action::ToggleQueueOverlay,
        Action::ToggleEntryCollapsed(EntryId(0)),
        Action::FocusEntry(EntryId(0)),
        Action::ExpandEntry(EntryId(0)),
        Action::OpenEntryInDetail(EntryId(0)),
        Action::QueueDelete(0),
        Action::QueueReorderBegin(0),
        Action::QueueUndo,
        Action::QueueEdit(0),
        Action::QueueRunNext(0),
        Action::JumpToLatest,
        Action::ScrollbarJump,
        Action::MinimapJump(EntryId(0)),
        Action::ConfirmPaste,
        Action::CancelPaste,
        Action::PasteTransformSelect(0),
        Action::PasteStagingSelect(0),
        Action::ClipboardSelect(0),
        Action::ClipboardRecopy(0),
        Action::ClipboardDelete(0),
        Action::ClipboardClear,
    ];
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
// Gesture recognizer
// ===========================================================================

/// A second/third press on the *same target key* within this window is treated
/// as a double/triple click. The single source of truth for the multi-click
/// window: the card-affordance recognizer (here) and the main-text selection
/// path (`handle_main_selection_press` in `lib.rs`, still cell-keyed) both read
/// this constant, so the two recognizers can never drift to different windows.
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
    /// Whether the first Drag event has already promoted this press into a drag
    /// (so `DragStart` fires exactly once; every later Drag is `DragExtend`,
    /// even when the pointer stays on the origin key — e.g. sub-row jitter).
    started: bool,
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
            started: false,
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
                drag.current = target;
                // The FIRST Drag event after a press promotes it into a drag
                // (DragStart, so the dispatch layer can arm its insertion
                // tracking); every subsequent Drag extends it — including ones
                // that land back on the origin key (sub-row jitter on a tall
                // row). `started` makes DragStart fire exactly once.
                if !drag.started {
                    drag.started = true;
                    Gesture::DragStart {
                        target: drag.origin,
                    }
                } else {
                    Gesture::DragExtend { target }
                }
            }
            None => {
                // A Drag with no recorded press (capture toggled mid-gesture):
                // start tracking from here so we don't desync. This Drag is the
                // promotion, so `started` is already true.
                self.drag = Some(DragState {
                    origin: target,
                    current: target,
                    started: true,
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

    /// Reset all in-flight gesture state — to be called when mouse capture
    /// turns off or the surface changes out from under an in-flight gesture.
    /// Not yet wired into a production path (stale recognizer state is currently
    /// harmless: `on_press` resets multiplicity on a target-key change, and the
    /// queue drag is gated on the separate `prompt_queue_drag` field); it lands
    /// in `handle_mouse`'s capture-toggle path with the hover/drag-capture
    /// affordances in a later phase. Exercised by the recognizer tests today.
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
