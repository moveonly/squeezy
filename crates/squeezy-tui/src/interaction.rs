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
    /// A snippet row in the Prompt Snippets picker overlay (§12.3.2), addressed
    /// by its stable per-snippet id (NOT its list index) so a delete/drop
    /// mid-gesture never shifts the hit target.
    SnippetEntry(u64),
    /// A template row in the Prompt Templates picker overlay (§12.3.6), addressed
    /// by its stable per-template id (NOT its list index) so a delete/drop
    /// mid-gesture never shifts the hit target.
    TemplateEntry(u64),
    /// A subagent timeline row (§12.8.2), addressed by its 0-based index into the
    /// pane's record list (row `index + 1`, since pane row 0 is `main`). Keyed by
    /// index rather than a screen cell so a pane reflow / scroll re-registers the
    /// same target at a fresh row; resolved against the live record list at
    /// activation time so a prune mid-gesture resolves to a no-op rather than the
    /// wrong subagent.
    SubagentRow(usize),
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
    /// The "Accept" action in the inline large-paste question (§11G.6).
    PasteConfirm,
    /// The "Discard" action in the inline large-paste question (§11G.6).
    PasteCancel,
    /// A row in the inline paste-transform question (§12.6.2), keyed by its 0-based index
    /// in the offered-transform list so a click selects exactly that shape.
    PasteTransformItem(usize),
    /// The "Re-copy" button in the clipboard-history picker (§12.6.1).
    ClipboardRecopy,
    /// The "Delete" button in the clipboard-history picker (§12.6.1).
    ClipboardDelete,
    /// The "Clear all" button in the clipboard-history picker (§12.6.1).
    ClipboardClear,
    /// An action button in the External Editor Handoff confirmation overlay
    /// (§12.6.5), keyed by its 0-based index in the accept/reopen/discard list so
    /// a click selects exactly that action.
    EditorHandoffItem(usize),
    /// The main-view Semantic Filter badge (§12.5.2) painted at the top-left of
    /// the transcript while a filter is active. A click cycles the filter forward
    /// — the mouse twin of the `Alt+f` keyboard verb.
    SemanticFilterBadge,
    /// A category row in the Local Transcript Index overlay (§12.5.1), keyed by
    /// its 0-based index in the populated-category list so a click selects (and a
    /// second click jumps within) exactly that category.
    TranscriptIndexRow(usize),
    /// A related-entry row in the Related-Entry Links overlay (§12.5.3), keyed by
    /// its 0-based index in the focused entry's ranked relation list so a click
    /// selects + jumps to exactly that related entry.
    RelatedLinkRow(usize),
    /// A fold-span row in the Duplicate-Output Folds overlay (§12.5.4), keyed by
    /// its 0-based index in the span list so a click selects (and a second click
    /// jumps to / expands) exactly that fold.
    DuplicateFoldRow(usize),
    /// An error-lens row in the Error Lenses overlay (§12.5.6), keyed by its
    /// 0-based index in the detected-lens list so a click selects + jumps to the
    /// failing entry behind exactly that lens.
    ErrorLensRow(usize),
    /// A health-marker row in the Transcript Health Markers overlay (§12.5.7),
    /// keyed by its 0-based index in the detected-marker list so a click selects
    /// + jumps to the entry behind exactly that marker.
    HealthMarkerRow(usize),
    /// A node row in the Semantic Turn Outline overlay (§12.2.1), keyed by its
    /// 0-based index in the outline-node list so a click selects + jumps the main
    /// view to the logical transcript row behind exactly that node.
    TurnOutlineRow(usize),
    /// A lane row in the Collapsible Reasoning/Tool Lanes overlay (§12.2.2), keyed
    /// by its 0-based index in the focused entry's lane list so a click selects +
    /// toggles the collapse state of exactly that lane.
    LaneFoldRow(usize),
    /// A bookmark row in the Reading Position Bookmarks overlay (§12.2.4), keyed
    /// by its 0-based index in the bookmark list so a click selects + jumps the
    /// main view to the entry that exact bookmark anchors.
    BookmarkRow(usize),
    /// An event row in the Session Timeline overlay (§12.2.6), keyed by its
    /// 0-based index in the *visible* (filtered) event list so a click selects +
    /// jumps the main view to the transcript row the event stands for.
    TimelineRow(usize),
    /// A subagent row in the Subagent Timeline Panel (§12.8.1), keyed by its
    /// 0-based index in the *visible* (filtered) subagent list so a click selects +
    /// jumps the main view to that subagent's conversation.
    SubagentTimelineRow(usize),
    /// The small `[promote]` affordance painted at the right of a Subagent Timeline
    /// Panel row (§12.8.4), keyed by the row's 0-based index in the *visible*
    /// (filtered) subagent list. A click promotes that subagent's result into a
    /// follow-up prompt (composer when idle / queue when a turn runs) rather than
    /// jumping — the mouse twin of the panel's `y` key.
    SubagentTimelinePromoteButton(usize),
    /// The small `[mark]` cell on a Subagent Timeline Panel row, keyed by the
    /// subagent's 0-based pane index, so a click marks / unmarks that subagent for
    /// the Compare Subagent Outputs view (§12.8.3).
    SubagentCompareMark(usize),
    /// A worker card on the Live Review Board (§12.8.5), keyed by the worker's
    /// stable subagent id (NOT a flattened index) so a click selects + jumps the
    /// main view to that worker's conversation even as lanes change between frames.
    ReviewBoardCard(u64),
    /// The Attention Routing indicator painted on the status line (§12.8.6). A
    /// single affordance with no identity of its own; a click quick-jumps to the
    /// subagent that most needs attention — the mouse twin of the `JumpToAttention`
    /// (`Ctrl+Alt+Z`) verb.
    AttentionIndicator,
    /// The Adaptive Density indicator painted on the status line (§12.4.1). A
    /// single affordance with no identity of its own; a click cycles the density
    /// override `auto → compact → default → expanded → auto` — the mouse twin of
    /// the `CycleDensity` (`Ctrl+Alt+X`) verb.
    DensityIndicator,
    /// The Presentation Mode indicator painted on the status line (§12.4.6). A
    /// single affordance with no identity of its own; a click toggles the mode
    /// on/off — the mouse twin of the `TogglePresentation` (`Ctrl+Alt+C`) verb.
    PresentationIndicator,
    /// The header of a docked auxiliary panel (§12.4.4). A single affordance with
    /// no identity of its own; a click cycles the panel's dock position
    /// `left → right → bottom → undocked` — the mouse twin of the `CycleDockPanel`
    /// (`Ctrl+Alt+F`) verb.
    DockPanelHeader,
    /// An annotation row in the Entry Annotations overlay (§12.2.5), keyed by its
    /// 0-based index in the annotation list so a click selects + jumps the main
    /// view to the entry that exact annotation anchors.
    AnnotationRow(usize),
    /// The small inline annotation marker painted on an annotated transcript
    /// entry's header row (§12.2.5), keyed by the entry's stable [`EntryId`] so a
    /// click opens the annotations overlay parked on that entry's note.
    EntryAnnotationMarker(EntryId),
    /// A change row in the What Changed Since Here? overlay (§12.2.7), keyed by its
    /// 0-based index in the flattened (grouped) change list so a click selects +
    /// jumps the main view to the transcript entry the change stands for.
    ChangeSinceRow(usize),
    /// An action row in the Contextual Action Palette (§12.1.2), keyed by its
    /// 0-based index in the gathered action list so a click selects + runs exactly
    /// that contextual action on the focused unit.
    ActionPaletteRow(usize),
    /// A command row in the Universal Command Palette overlay (§12.1.1), keyed by
    /// its 0-based index in the *visible* (fuzzy-filtered) command list so a click
    /// selects + runs exactly that command.
    CommandPaletteRow(usize),
    /// A crumb in the Clickable Breadcrumbs strip (§12.1.5), keyed by its 0-based
    /// index in the trail (root-first) so a click focuses + activates exactly that
    /// crumb's navigation target.
    BreadcrumbCrumb(usize),
    /// The small inline rename-label badge painted on a transcript entry's header
    /// row (§12.1.7), keyed by the entry's stable [`EntryId`] so a click opens the
    /// inline rename editor on that entry's label.
    RenameLabel(EntryId),
    /// The dim Gentle First-Run Interaction Hint strip (§12.1.8). A single
    /// affordance with no identity of its own; a left click anywhere on the line
    /// dismisses the shown hint — the mouse twin of the `DismissFirstRunHint` verb.
    FirstRunHint,
    /// The Automatic Degraded-Mode Suggestions banner's `[accept]` affordance
    /// (§12.9.4). A click applies the suggested degraded modes — the mouse twin of
    /// the `AcceptDegradedSuggestion` verb.
    DegradedSuggestionAccept,
    /// The Automatic Degraded-Mode Suggestions banner's `[dismiss]` affordance
    /// (§12.9.4). A click latches the suggestion dismissed — the mouse twin of the
    /// `DismissDegradedSuggestion` verb.
    DegradedSuggestionDismiss,
    /// The "Insert" button in the Prompt Snippets picker (§12.3.2).
    SnippetInsert,
    /// The "Queue" button in the Prompt Snippets picker (§12.3.2).
    SnippetQueue,
    /// The "Delete" button in the Prompt Snippets picker (§12.3.2).
    SnippetDelete,
    /// The "Clear all" button in the Prompt Snippets picker (§12.3.2).
    SnippetClear,
    /// An item row in the Actionable Tool Outputs overlay (§12.3.1), keyed by its
    /// 0-based index in the detected-item list so a click selects + runs the item's
    /// primary action (copy) on exactly that element.
    ToolActionsRow(usize),
    /// The "Insert to composer" button in the Scratchpad Pane (§12.3.3).
    ScratchpadInsert,
    /// The "Queue" button in the Scratchpad Pane (§12.3.3).
    ScratchpadQueue,
    /// The "Append selection / source link" button in the Scratchpad Pane (§12.3.3).
    ScratchpadAppend,
    /// The "Clear" button in the Scratchpad Pane (§12.3.3).
    ScratchpadClear,
    /// A slot row in an open Prompt Template card (§12.3.6), keyed by its 0-based
    /// index in the card's slot list so a click focuses exactly that slot for
    /// editing.
    TemplateSlotRow(usize),
    /// The "Enqueue" button in the Prompt Templates card (§12.3.6) — resolves the
    /// filled card and stages it onto the prompt queue.
    TemplateEnqueue,
    /// The "Delete" button in the Prompt Templates picker (§12.3.6).
    TemplateDelete,
    /// The "Clear all" button in the Prompt Templates picker (§12.3.6).
    TemplateClear,
    /// The Replayable Interaction Macros (§12.3.7) record/replay status strip. A
    /// single affordance with no identity of its own; a left click on the line
    /// stops/cancels the active recording or replay — the mouse twin of the
    /// `ToggleMacroRecord` verb.
    MacroStrip,
    /// An action row in the Keybinding Editor UI overlay (§12.7.1), keyed by its
    /// 0-based index in the editor's row list so a click selects (and, on the
    /// already-selected row, begins capturing) exactly that action.
    KeybindingRow(usize),
    /// The "Rebind" button in the Keybinding Editor UI (§12.7.1) — the mouse twin
    /// of the keyboard Enter verb; begins capturing a new chord for the selected
    /// row.
    KeybindingRebind,
    /// The "Reset" button in the Keybinding Editor UI (§12.7.1) — the mouse twin
    /// of the keyboard `r`/Delete verb; reverts the selected row to its default.
    KeybindingReset,
    /// A palette-role row in the Theme Editor overlay (§12.7.2), keyed by its
    /// 0-based index in the editor's curated role list so a click focuses exactly
    /// that role (reseeding the working swatch from its live colour).
    ThemeEditorRole(usize),
    /// A channel bar (R/G/B) in the Theme Editor overlay (§12.7.2), keyed by its
    /// 0-based channel index (0=R, 1=G, 2=B) so a click on a point along the bar
    /// focuses that channel and sets its value to the clicked position.
    ThemeEditorChannel(usize),
    /// A field row in the Per-Workspace UI Profile overlay (§12.7.4), keyed by its
    /// 0-based index in the overlay's field list so a click focuses exactly that
    /// row (the mouse twin of ↑↓).
    WorkspaceProfileField(usize),
    /// A profile-field row in the Per-Terminal Profiles overlay (§12.7.3), keyed by
    /// its 0-based index in the editor's field list (0=glyphs, 1=mouse, 2=color) so
    /// a click focuses that field and cycles its value.
    TerminalProfileField(usize),
    /// A gesture-field row in the Gesture Settings overlay (§12.7.5), keyed by its
    /// 0-based index in the editor's field list so a click focuses that field and
    /// steps its value forward.
    GestureSettingsField(usize),
    /// A mode row in the Minimal Glyph Mode overlay (§12.7.6), keyed by its 0-based
    /// index in [`crate::glyph_mode::GlyphMode::ALL`] (0=Unicode, 1=Compact,
    /// 2=ASCII) so a click selects exactly that mode (the mouse twin of ↑↓).
    GlyphModeRow(usize),
    /// A field row in the Smart Split Panes overlay (§12.4.2), keyed by its 0-based
    /// index in [`crate::smart_split::SplitField::ALL`] (0=pane kind, 1=orientation,
    /// 2=split ratio) so a click focuses + adjusts exactly that row (the mouse twin
    /// of ↑↓ + ←→/Space).
    SmartSplitField(usize),
    /// The Zen Mode (§12.4.5) minimal status line painted where the detailed
    /// status block would sit while zen is on. A single affordance with no identity
    /// of its own; a click anywhere on it leaves zen — the mouse twin of the
    /// `ToggleZenMode` (`Ctrl+Alt+.`) verb.
    ZenStatusLine,
    /// The `[restore]` affordance painted in the Session Auto-Save Checkpoints
    /// overlay (§12.9.5). A single affordance with no identity of its own; a click
    /// on it restores the saved checkpoint onto the running session — the mouse
    /// twin of the overlay's `r` verb.
    CheckpointRestore,
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
    /// Cycle the given queue item's run-condition (§12.3.5) to the next one in the
    /// editor sequence. The mouse twin of the keyboard `v` verb; both route
    /// through the same `queue_cycle_condition_by_id`, so the paths stay identical
    /// by construction. Fed by a Ctrl+Right-click on a queue-item row (the
    /// always-available equivalent is the keyboard `v`, since crossterm only
    /// reports mouse modifiers when key-modifier capture is on).
    QueueCycleCondition(u64),
    /// Jump the transcript to the latest (tail) row.
    JumpToLatest,
    /// Jump the scrollbar thumb to the clicked gutter row.
    ScrollbarJump,
    /// Jump the transcript so the entry behind a minimap turn-rail cell sits at
    /// the top of the viewport. Keyed by the cell's [`EntryId`] so a resize
    /// re-registers the same target at a fresh rail cell.
    MinimapJump(EntryId),
    /// Confirm the pending large paste in the inline question (§11G.6),
    /// inserting it into the composer. Mouse twin of Enter/`y`.
    ConfirmPaste,
    /// Cancel the pending large paste in the inline question (§11G.6),
    /// discarding it. Mouse twin of Esc/`n`.
    CancelPaste,
    /// Select (move the cursor to) the given row in the inline paste-transform question
    /// (§12.6.2) and apply it. Mouse twin of moving the cursor with ↑↓ and
    /// pressing Enter; a click both selects and applies the shape in one go.
    PasteTransformSelect(usize),
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
    /// Select (move the cursor to) the given action in the External Editor
    /// Handoff confirmation overlay (§12.6.5) and apply it. Mouse twin of moving
    /// the cursor with ↑↓ and pressing Enter; a click both selects and applies
    /// the accept/reopen/discard action in one go.
    EditorHandoffSelect(usize),
    /// Cycle the main-view Semantic Filter (§12.5.2) forward one category. Mouse
    /// twin of the `Alt+f` keyboard verb / a click on the active-filter badge;
    /// both step the filter through its cycle and request a redraw.
    CycleSemanticFilter,
    /// Quick-jump to the subagent that most needs attention (§12.8.6). Mouse twin
    /// of the `JumpToAttention` (`Ctrl+Alt+Z`) keyboard verb / a click on the
    /// status-line attention indicator; both land on the single highest-priority
    /// attention target.
    JumpToAttention,
    /// Cycle the Adaptive Density override (§12.4.1). Mouse twin of the
    /// `CycleDensity` (`Ctrl+Alt+X`) keyboard verb / a click on the status-line
    /// density indicator; both step the override `auto → compact → default →
    /// expanded → auto`, persist it, and request a redraw.
    CycleDensity,
    /// Toggle Presentation Mode (§12.4.6) on/off. Mouse twin of the
    /// `TogglePresentation` (`Ctrl+Alt+C`) keyboard verb / a click on the
    /// status-line `[present]` indicator; both flip the screen-share display mode,
    /// persist it, and request a redraw.
    TogglePresentation,
    /// Cycle the active Dockable Panel's dock position (§12.4.4). Mouse twin of
    /// the `CycleDockPanel` (`Ctrl+Alt+F`) keyboard verb / a click on the docked
    /// panel's header; both step the panel `left → right → bottom → undocked`,
    /// persist it, and request a redraw.
    CycleDockPanel,
    /// Toggle Zen Mode (§12.4.5). Mouse twin of the `ToggleZenMode` (`Ctrl+Alt+.`)
    /// keyboard verb / a click on the minimal zen status line; both flip the
    /// distraction-free latch, persist it, and request a redraw.
    ToggleZenMode,
    /// Select the given category row in the Local Transcript Index overlay
    /// (§12.5.1) and jump the main view to the next entry in it. Mouse twin of
    /// moving the cursor with ↑↓ and pressing Enter; a click both selects and
    /// jumps in one go.
    TranscriptIndexSelect(usize),
    /// Select the given related-entry row in the Related-Entry Links overlay
    /// (§12.5.3) and jump the main view to it. Mouse twin of moving the cursor
    /// with ↑↓ and pressing Enter; a click both selects and jumps in one go.
    RelatedLinkSelect(usize),
    /// Select the given fold-span row in the Duplicate-Output Folds overlay
    /// (§12.5.4): move the cursor onto it, jump the main view to its lead, and
    /// toggle the span expanded/collapsed. Mouse twin of moving the cursor with
    /// ↑↓ and pressing Enter; a click selects, jumps, and toggles in one go.
    DuplicateFoldSelect(usize),
    /// Select the given error-lens row in the Error Lenses overlay (§12.5.6):
    /// move the cursor onto it and jump the main view to the failing entry behind
    /// it. Mouse twin of moving the cursor with ↑↓ and pressing Enter; a click
    /// both selects and jumps in one go.
    ErrorLensSelect(usize),
    /// Select the given health-marker row in the Transcript Health Markers
    /// overlay (§12.5.7): move the cursor onto it and jump the main view to the
    /// entry behind it. Mouse twin of moving the cursor with ↑↓ and pressing
    /// Enter; a click both selects and jumps in one go.
    HealthMarkerSelect(usize),
    /// Select the given node row in the Semantic Turn Outline overlay (§12.2.1):
    /// move the cursor onto it and jump the main view to the logical transcript
    /// row behind it. Mouse twin of moving the cursor with ↑↓ and pressing Enter;
    /// a click both selects and jumps in one go.
    TurnOutlineSelect(usize),
    /// Select the given lane row in the Collapsible Reasoning/Tool Lanes overlay
    /// (§12.2.2): move the cursor onto it and toggle that lane's collapsed state.
    /// Mouse twin of moving the cursor with ↑↓ and pressing Enter/Space; a click
    /// both selects and folds/unfolds the lane in one go.
    LaneFoldToggle(usize),
    /// Select the given bookmark row in the Reading Position Bookmarks overlay
    /// (§12.2.4): move the cursor onto it and jump the main view to the entry that
    /// bookmark anchors. Mouse twin of moving the cursor with ↑↓ and pressing
    /// Enter; a click both selects and jumps in one go.
    BookmarkSelectJump(usize),
    /// Select the given event row in the Session Timeline overlay (§12.2.6): move
    /// the cursor onto it and jump the main view to the transcript row the event
    /// stands for. Mouse twin of moving the cursor with ↑↓ and pressing Enter; a
    /// click both selects and jumps in one go. The index is into the *visible*
    /// (filtered) event list.
    TimelineSelectJump(usize),
    /// Select the given subagent row in the Subagent Timeline Panel (§12.8.1):
    /// move the cursor onto it and jump the main view to that subagent's
    /// conversation. Mouse twin of moving the cursor with ↑↓ and pressing Enter; a
    /// click both selects and jumps in one go. The index is into the *visible*
    /// (filtered) subagent list.
    SubagentTimelineSelectJump(usize),
    /// Promote the given subagent row's result into a follow-up prompt (§12.8.4):
    /// distill its completion summary / failure diagnostic / latest activity into a
    /// clean plain-text prompt and fill the composer (idle) or queue it (active
    /// turn) — never auto-submitted. Mouse twin of the panel's `y` promote key + the
    /// `PromoteSubagentResult` keyboard verb; both reach the same
    /// `promote_subagent_timeline_row` handler. The index is into the *visible*
    /// (filtered) subagent list, matching the row's select target.
    SubagentTimelinePromote(usize),
    /// Toggle the given subagent's mark for the Compare Subagent Outputs view
    /// (§12.8.3): the mouse twin of pressing `c` on the row in the Subagent
    /// Timeline Panel. Marking two subagents lets the compare view open over them.
    /// The index is the 0-based pane index of the subagent record.
    SubagentCompareMark(usize),
    /// Select the given worker card on the Live Review Board (§12.8.5): park the
    /// cursor on it (by stable id) and jump the main view to that worker's
    /// conversation. Mouse twin of walking the cursor with ↑↓ and pressing Enter; a
    /// click both selects and jumps in one go. Keyed by the worker's stable subagent
    /// id, matching the card's click target.
    ReviewBoardSelectJump(u64),
    /// Select the given annotation row in the Entry Annotations overlay (§12.2.5):
    /// move the cursor onto it and jump the main view to the entry that annotation
    /// anchors. Mouse twin of moving the cursor with ↑↓ and pressing Enter; a click
    /// both selects and jumps in one go.
    AnnotationSelectJump(usize),
    /// Open the Entry Annotations overlay (§12.2.5) parked on the first annotation
    /// of the given entry — the mouse twin of clicking the inline annotation marker
    /// on an entry's header row. Keyed by the entry's stable [`EntryId`] so the
    /// target survives resize.
    OpenAnnotationsForEntry(EntryId),
    /// Select the given change row in the What Changed Since Here? overlay
    /// (§12.2.7): move the cursor onto it and jump the main view to the transcript
    /// entry the change stands for. Mouse twin of moving the cursor with ↑↓ and
    /// pressing Enter; a click both selects and jumps in one go. The index is into
    /// the flattened (grouped) change list.
    ChangeSinceSelectJump(usize),
    /// Select the given action row in the Contextual Action Palette (§12.1.2):
    /// move the cursor onto it and run that contextual action on the focused unit.
    /// Mouse twin of moving the cursor with ↑↓ and pressing Enter; a click both
    /// selects and runs the action in one go. The index is into the gathered
    /// action list.
    PaletteActionRun(usize),
    /// Select the given command row in the Universal Command Palette overlay
    /// (§12.1.1) and run it. Mouse twin of moving the cursor with ↑↓ and pressing
    /// Enter; a click both selects and runs the command in one go. The index is into
    /// the *visible* (fuzzy-filtered) command list.
    CommandPaletteRun(usize),
    /// Focus + activate the given crumb in the Clickable Breadcrumbs strip
    /// (§12.1.5): move the breadcrumb focus onto it and jump to the location it
    /// stands for. Mouse twin of moving the focus with ←→ and pressing Enter; a
    /// click both focuses and jumps in one go. The index is into the trail
    /// (root-first).
    BreadcrumbActivate(usize),
    /// Open the inline rename editor for the given transcript entry (§12.1.7),
    /// seeded with its current label. Mouse twin of focusing the entry and pressing
    /// the rename chord; a click on the entry's label badge both targets it and
    /// opens the editor in one go.
    OpenRenameForEntry(EntryId),
    /// Dismiss the shown Gentle First-Run Interaction Hint (§12.1.8). Mouse twin of
    /// the `DismissFirstRunHint` keyboard verb; a click on the dim hint strip retires
    /// the hint (latched seen for the session) so it never returns.
    DismissFirstRunHint,
    /// Accept the shown Automatic Degraded-Mode Suggestion (§12.9.4). Mouse twin of
    /// the `AcceptDegradedSuggestion` keyboard verb; a click on the banner's
    /// `[accept]` affordance applies the suggested ASCII chrome / compact density /
    /// mouse-off modes to the live session and latches the suggestion.
    AcceptDegradedSuggestion,
    /// Dismiss the shown Automatic Degraded-Mode Suggestion (§12.9.4). Mouse twin of
    /// the `DismissDegradedSuggestion` keyboard verb; a click on the banner's
    /// `[dismiss]` affordance latches it for the session so it never returns.
    DismissDegradedSuggestion,
    /// Select the given snippet (by stable id) in the Prompt Snippets picker
    /// (§12.3.2). Mouse twin of the picker's Up/Down arrows. Fed by a single click
    /// on a snippet row.
    SnippetSelect(u64),
    /// Insert the given snippet (by stable id) into the composer (§12.3.2). Mouse
    /// twin of the picker's Enter verb / the "Insert" button. Fed by a double-click
    /// on a snippet row.
    SnippetInsertCompose(u64),
    /// Stage the given snippet (by stable id) onto the prompt queue (§12.3.2).
    /// Mouse twin of the picker's `q` verb / the "Queue" button.
    SnippetEnqueue(u64),
    /// Delete the given snippet (by stable id) from the store (§12.3.2). Mouse
    /// twin of the picker's `d` verb / the "Delete" button.
    SnippetDelete(u64),
    /// Clear every saved snippet (§12.3.2). Mouse twin of the picker's `c` verb /
    /// the "Clear all" button.
    SnippetClear,
    /// Select the given item row in the Actionable Tool Outputs overlay (§12.3.1)
    /// and run its primary action (copy the matched element to the clipboard). Mouse
    /// twin of moving the cursor with ↑↓ and pressing Enter; a click both selects and
    /// copies in one go. The index is into the detected-item list.
    ToolActionRun(usize),
    /// Insert the whole scratchpad buffer into the composer (§12.3.3). Mouse twin
    /// of the pane's `Ctrl+I` verb / the "Insert to composer" button.
    ScratchpadInsertCompose,
    /// Queue the whole scratchpad buffer as a prompt (§12.3.3). Mouse twin of the
    /// pane's `Ctrl+Q` verb / the "Queue" button.
    ScratchpadEnqueue,
    /// Append the active selection / a source link to the scratchpad (§12.3.3).
    /// Mouse twin of the pane's `Ctrl+L` verb / the "Append" button.
    ScratchpadAppend,
    /// Clear the scratchpad buffer and its source links (§12.3.3). Mouse twin of
    /// the pane's `Ctrl+K` verb / the "Clear" button.
    ScratchpadClear,
    /// Select the given template (by stable id) in the Prompt Templates picker
    /// (§12.3.6) and instantiate it into an editable card. Mouse twin of the
    /// picker's Up/Down + Enter. Fed by a click on a template row.
    TemplateSelect(u64),
    /// Focus the given slot (by 0-based index) in the open Prompt Template card
    /// (§12.3.6) for editing. Mouse twin of the card's Tab / ↑↓ slot movement; fed
    /// by a click on a slot row.
    TemplateFocusSlot(usize),
    /// Resolve the filled Prompt Template card and stage it onto the prompt queue
    /// (§12.3.6). Mouse twin of the card's Enter verb / the "Enqueue" button.
    /// Blocked (with inline status) while any slot is still empty.
    TemplateEnqueue,
    /// Delete the picker's selected template (by stable id) from the store
    /// (§12.3.6). Mouse twin of the picker's `d` verb / the "Delete" button.
    TemplateDelete(u64),
    /// Clear every saved template (§12.3.6). Mouse twin of the picker's `c` verb /
    /// the "Clear all" button.
    TemplateClear,
    /// A click on the Replayable Interaction Macros (§12.3.7) status strip: stop /
    /// cancel the active recording or replay. Mouse twin of the `ToggleMacroRecord`
    /// keyboard verb; both route to the same `toggle_macro_record` handler, so
    /// keyboard/mouse parity holds by construction.
    MacroToggleRecord,
    /// Select the given action row (by 0-based index) in the Keybinding Editor UI
    /// overlay (§12.7.1). Mouse twin of the editor's ↑↓/kj cursor movement; fed by
    /// a click on a row that is not already selected.
    KeybindingSelect(usize),
    /// Begin capturing a new chord for the editor's selected row (§12.7.1). Mouse
    /// twin of the editor's Enter verb / the "Rebind" button / a click on the
    /// already-selected row. Routes through the same `begin_capture` the keyboard
    /// path drives, so keyboard/mouse parity holds by construction.
    KeybindingRebind,
    /// Reset the editor's selected row to its compiled-in default (§12.7.1). Mouse
    /// twin of the editor's `r`/Delete verb / the "Reset" button.
    KeybindingReset,
    /// Focus the given palette role (by 0-based index in the editor's role list) in
    /// the Theme Editor overlay (§12.7.2), reseeding the working swatch from its
    /// live colour. Mouse twin of moving the role focus with ↑↓; a click on a role
    /// row both selects it and shows its colour in one go.
    ThemeEditorSelectRole(usize),
    /// Set the focused channel of the Theme Editor overlay (§12.7.2) to an absolute
    /// value by clicking a point along its bar. Carries the channel index (0=R,
    /// 1=G, 2=B) and the 0..=255 value the clicked column maps to. Mouse twin of
    /// focusing a channel with ←→ and nudging it with +/- to the same value, with
    /// the same live preview.
    ThemeEditorSetChannel(usize, u8),
    /// Focus the given field row (by 0-based index in the overlay's field list) in
    /// the Per-Workspace UI Profile overlay (§12.7.4). Mouse twin of moving the
    /// field focus with ↑↓; a click on a row selects it.
    WorkspaceProfileSelectField(usize),
    /// Focus + cycle the given profile field (by 0-based index in the editor's
    /// field list) in the Per-Terminal Profiles overlay (§12.7.3). Mouse twin of
    /// moving the field focus with ↑↓ and cycling its value with ←→/Space; a click
    /// on a field row both focuses it and advances its value in one go.
    TerminalProfileCycleField(usize),
    /// Focus + step the given gesture field (by 0-based index in the editor's field
    /// list) forward in the Gesture Settings overlay (§12.7.5). Mouse twin of moving
    /// the field focus with ↑↓ and stepping its value with ←→/Space/+/-; a click on a
    /// field row both focuses it and advances its value in one go.
    GestureSettingsStepField(usize),
    /// Select the given glyph mode (by 0-based index in
    /// [`crate::glyph_mode::GlyphMode::ALL`]) in the Minimal Glyph Mode overlay
    /// (§12.7.6). Mouse twin of moving the row focus with ↑↓; a click on a mode
    /// row selects it (and live-previews it).
    GlyphModeSelect(usize),
    /// Select / pin the given subagent (by 0-based pane index) as the active
    /// comparison target (§12.8.2). Mouse twin of moving the pane cursor with ↑↓;
    /// a single click both moves the cursor onto that row and previews it. Routes
    /// through the same `subagent_select_index` the keyboard path reaches, so
    /// keyboard/mouse parity holds by construction.
    SubagentSelect(usize),
    /// Jump to the given subagent's transcript / detail pane (by 0-based pane
    /// index; §12.8.2), preserving the prior conversation + scroll as a return
    /// anchor. Mouse twin of the keyboard `JumpToSubagent` verb; a double-click on
    /// a subagent row reaches the same `jump_to_subagent_index` handler the verb
    /// does. A capped subagent (no transcript) resolves to a select-only no-op.
    SubagentJump(usize),
    /// Focus + adjust the given field row (by 0-based index in
    /// [`crate::smart_split::SplitField::ALL`]) in the Smart Split Panes overlay
    /// (§12.4.2). Mouse twin of moving the field focus with ↑↓ and adjusting it with
    /// ←→/Space; a click on a field row both focuses it and steps its value forward
    /// in one go (cycle the pane kind / orientation, or widen the split).
    SmartSplitAdjustField(usize),
    /// Restore the saved UI-state checkpoint onto the running session (§12.9.5).
    /// Mouse twin of the Session Auto-Save Checkpoints overlay's `r` verb, driving
    /// the same `restore_session_checkpoint` handler so keyboard/mouse parity holds
    /// by construction. A click on the overlay's `[restore]` affordance reaches it.
    CheckpointRestore,
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
        Action::QueueCycleCondition(0),
        Action::JumpToLatest,
        Action::ScrollbarJump,
        Action::MinimapJump(EntryId(0)),
        Action::ConfirmPaste,
        Action::CancelPaste,
        Action::PasteTransformSelect(0),
        Action::ClipboardSelect(0),
        Action::ClipboardRecopy(0),
        Action::ClipboardDelete(0),
        Action::ClipboardClear,
        Action::EditorHandoffSelect(0),
        Action::CycleSemanticFilter,
        Action::TranscriptIndexSelect(0),
        Action::RelatedLinkSelect(0),
        Action::DuplicateFoldSelect(0),
        Action::ErrorLensSelect(0),
        Action::HealthMarkerSelect(0),
        Action::TurnOutlineSelect(0),
        Action::LaneFoldToggle(0),
        Action::BookmarkSelectJump(0),
        Action::TimelineSelectJump(0),
        Action::AnnotationSelectJump(0),
        Action::OpenAnnotationsForEntry(EntryId(0)),
        Action::ChangeSinceSelectJump(0),
        Action::PaletteActionRun(0),
        Action::CommandPaletteRun(0),
        Action::BreadcrumbActivate(0),
        Action::OpenRenameForEntry(EntryId(0)),
        Action::DismissFirstRunHint,
        Action::AcceptDegradedSuggestion,
        Action::DismissDegradedSuggestion,
        Action::SnippetSelect(0),
        Action::SnippetInsertCompose(0),
        Action::SnippetEnqueue(0),
        Action::SnippetDelete(0),
        Action::SnippetClear,
        Action::ToolActionRun(0),
        Action::ScratchpadInsertCompose,
        Action::ScratchpadEnqueue,
        Action::ScratchpadAppend,
        Action::ScratchpadClear,
        Action::TemplateSelect(0),
        Action::TemplateFocusSlot(0),
        Action::TemplateEnqueue,
        Action::TemplateDelete(0),
        Action::TemplateClear,
        Action::MacroToggleRecord,
        Action::KeybindingSelect(0),
        Action::KeybindingRebind,
        Action::KeybindingReset,
        Action::ThemeEditorSelectRole(0),
        Action::ThemeEditorSetChannel(0, 0),
        Action::WorkspaceProfileSelectField(0),
        Action::TerminalProfileCycleField(0),
        Action::GestureSettingsStepField(0),
        Action::GlyphModeSelect(0),
        Action::SubagentSelect(0),
        Action::SubagentJump(0),
        Action::SubagentTimelinePromote(0),
        Action::SubagentCompareMark(0),
        Action::ReviewBoardSelectJump(0),
        Action::JumpToAttention,
        Action::SmartSplitAdjustField(0),
        Action::CycleDensity,
        Action::TogglePresentation,
        Action::CycleDockPanel,
        Action::ToggleZenMode,
        Action::CheckpointRestore,
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

    /// The rect of the FIRST target registered this frame whose key equals
    /// `key`, if any. Used by the Hover Preview popover (§12.1.4) to anchor itself
    /// next to the previewed entry's on-screen row — the entry header registers a
    /// `TargetKey::Entry` rect each frame, so this resolves the live screen row
    /// from the same id-keyed registry the click path uses (never a stale
    /// coordinate). Returns `None` when the entry is scrolled off-screen.
    pub(crate) fn rect_for_key(&self, key: TargetKey) -> Option<Rect> {
        self.hits.iter().find(|h| h.key == key).map(|h| h.rect)
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
pub(crate) const HOVER_INTENT_MS: u128 = 90;

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
