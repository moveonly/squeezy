//! Last-Known-Good Layout Fallback (§12.9.3).
//!
//! The fullscreen `render()` path resolves the main view's vertical layout —
//! the transcript viewport, the composer, the task/approval/attachment blocks —
//! from the live frame size every paint. That math is normally robust, but a
//! pathological combination (a sub-degenerate terminal size, an
//! over-constrained reserve, a transient state where every block collapses to
//! zero rows) can yield a *degenerate* layout: a frame with no composer and no
//! transcript, which paints as an unusable blank surface. There is no native
//! signal for "this frame's geometry is broken" — ratatui will happily clip a
//! zero-height composer and commit the empty frame.
//!
//! [`LastGoodLayout`] is that signal. It rides the geometry the renderer
//! already computes (one [`LayoutGeometry`] snapshot per paint, all `Copy`
//! fields, no allocation) and does two things every frame:
//!
//! - **records** the geometry of each *valid* frame as the new last-known-good
//!   snapshot (keyed by the size it was painted at), and
//! - **falls back** — when the freshly-computed geometry is *degenerate* and a
//!   last-known-good snapshot exists for the *same size* — to that prior good
//!   geometry, so the renderer repaints the last frame that actually worked
//!   rather than committing the broken one.
//!
//! ## Validity check
//!
//! A geometry is [`LayoutGeometry::is_degenerate`] for its area when the area is
//! large enough to host the main view yet the layout reserves no composer row
//! (`input_height == 0`) — the composer is the one block the main view can never
//! lose — or its reserved blocks overflow the area height (the layout asked for
//! more rows than exist, which clips the bottom). Either condition means the
//! painted frame would be unusable. On a genuinely tiny area (too small for any
//! usable layout) nothing is degenerate: there is no better frame to fall back
//! to, so the renderer paints what it has.
//!
//! ## Size seam
//!
//! The snapshot is keyed by the `(width, height)` the geometry was computed for,
//! the same size seam the renderer stamps via `stamp_frame_size` /
//! `last_frame_size`. A fallback only substitutes a snapshot taken at the
//! *exact* size of the current frame, so a resize never paints stale geometry —
//! a resized frame with no matching good snapshot simply paints its own
//! (best-effort) layout and, if valid, becomes the new good snapshot for that
//! size.
//!
//! ## Idle-redraw contract
//!
//! Everything here is driven from the one geometry value the renderer already
//! has. A valid frame stores a `Copy` snapshot (one `Cell` write); an idle
//! session that never repaints pays nothing. There is no clock, no thread, and
//! no allocation, so the zero-idle-cost contract holds.

#![cfg_attr(not(unix), allow(dead_code))]

use std::cell::Cell;

/// A `Copy` snapshot of the main view's resolved vertical layout geometry, plus
/// the `(width, height)` it was computed against. Mirrors the renderer's
/// `MainTranscriptLayout` field-for-field so a recorded good frame can be
/// substituted back verbatim. All fields are `Copy`, so storing/restoring a
/// snapshot is a single move with no allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LayoutGeometry {
    /// Frame width the geometry was resolved for. The fallback only restores a
    /// snapshot whose size matches the current frame exactly.
    pub(crate) width: u16,
    /// Frame height the geometry was resolved for.
    pub(crate) height: u16,
    /// Optional task-panel height (`None` when the panel is hidden).
    pub(crate) task_height: Option<u16>,
    /// Approval menu height (0 when no approval is pending).
    pub(crate) approval_height: u16,
    /// Plan-mode indicator height (0 when not in plan mode).
    pub(crate) plan_indicator_height: u16,
    /// Subagent pane height (0 when no pane is shown).
    pub(crate) subagent_height: u16,
    /// Attachment panel height (0 when no attachments).
    pub(crate) attachment_height: u16,
    /// Transcript-to-prompt breather height (0 on an empty session).
    pub(crate) transcript_prompt_gap_height: u16,
    /// Transcript viewport height.
    pub(crate) transcript_height: u16,
    /// Whether the completed-turn divider is shown.
    pub(crate) show_completed_turn_divider: bool,
    /// Composer (input panel) height. The one block the main view can never
    /// lose; a zero here on a usable area is the canonical degenerate frame.
    pub(crate) input_height: u16,
}

/// The smallest frame height at which the main view is expected to host a usable
/// layout. Below this the terminal is too small for the transcript/composer
/// split to mean anything, so no layout is judged degenerate (there is nothing
/// better to fall back to) and the renderer paints whatever it resolved.
pub(crate) const MIN_USABLE_HEIGHT: u16 = 4;

/// The smallest frame width at which the main view is expected to host a usable
/// layout. Mirrors [`MIN_USABLE_HEIGHT`] for the horizontal axis.
pub(crate) const MIN_USABLE_WIDTH: u16 = 8;

/// Rows the render path always reserves for the fixed status block, on top of
/// the tracked main-column blocks. The renderer pushes a `Constraint::Length`
/// of this height and `main_transcript_layout` accounts for it in its required
/// reserve; [`reserved_height`] folds it in so the overflow check matches what
/// the renderer actually places.
///
/// [`reserved_height`]: LayoutGeometry::reserved_height
pub(crate) const STATUS_BLOCK_HEIGHT: u16 = 2;

impl LayoutGeometry {
    /// Whether the area is large enough that a usable main-view layout is
    /// expected. On a tinier area the validity check is disabled — see
    /// [`LayoutGeometry::is_degenerate`].
    pub(crate) fn area_is_usable(width: u16, height: u16) -> bool {
        width >= MIN_USABLE_WIDTH && height >= MIN_USABLE_HEIGHT
    }

    /// Total rows this geometry reserves in the main column. Saturating so an
    /// absurd combination can never wrap; the renderer's own layout uses the
    /// same saturating arithmetic, so this mirrors what it will try to place.
    pub(crate) fn reserved_height(self) -> u16 {
        self.task_height
            .unwrap_or(0)
            .saturating_add(self.approval_height)
            .saturating_add(self.plan_indicator_height)
            .saturating_add(self.subagent_height)
            .saturating_add(self.attachment_height)
            .saturating_add(self.transcript_prompt_gap_height)
            .saturating_add(self.transcript_height)
            .saturating_add(self.input_height)
            // Fixed status block the render path always pushes on top of the
            // tracked blocks; counted so the overflow check matches the frame.
            .saturating_add(STATUS_BLOCK_HEIGHT)
    }

    /// Whether this geometry would paint a degenerate (unusable) frame for its
    /// own area.
    ///
    /// Two conditions, checked only on an area large enough to host a usable
    /// layout (see [`area_is_usable`]):
    ///
    /// - **no composer** — `input_height == 0`. The composer is the one block
    ///   the main view always reserves; losing it leaves no way to type.
    /// - **overflow** — the reserved rows exceed the area height, so ratatui
    ///   would clip the bottom block off the screen. The reserved total
    ///   includes the fixed status block the render path always places (see
    ///   [`STATUS_BLOCK_HEIGHT`]), so the check matches the painted frame.
    ///
    /// On a sub-usable area nothing is degenerate: a 2-row terminal genuinely
    /// cannot host the split, and there is no better frame to restore, so the
    /// renderer paints what it has.
    ///
    /// [`area_is_usable`]: LayoutGeometry::area_is_usable
    pub(crate) fn is_degenerate(self) -> bool {
        if !Self::area_is_usable(self.width, self.height) {
            return false;
        }
        self.input_height == 0 || self.reserved_height() > self.height
    }
}

/// What [`LastGoodLayout::resolve`] decided the renderer should paint this
/// frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutResolution {
    /// Paint the freshly-computed geometry. Either it is valid (and has just
    /// become the new last-known-good snapshot) or it is degenerate but no
    /// matching good snapshot exists, so it is the best frame available.
    Use(LayoutGeometry),
    /// Paint the last-known-good geometry instead of the freshly-computed one:
    /// the current geometry is degenerate and a good snapshot at the same size
    /// is available, so the renderer repaints the last frame that worked.
    Fallback(LayoutGeometry),
}

impl LayoutResolution {
    /// The geometry the renderer should actually paint, regardless of whether it
    /// is the fresh one or the restored good snapshot.
    pub(crate) fn geometry(self) -> LayoutGeometry {
        match self {
            LayoutResolution::Use(g) | LayoutResolution::Fallback(g) => g,
        }
    }

    /// Whether this resolution substituted the last-known-good snapshot.
    // Asserted by the unit tests; the render path drives the substitution off
    // `geometry()` and reads the running count from the diagnostics line, so the
    // predicate itself is test/diagnostics surface rather than a render hook.
    #[allow(dead_code)]
    pub(crate) fn is_fallback(self) -> bool {
        matches!(self, LayoutResolution::Fallback(_))
    }
}

/// The last-known-good layout store for the fullscreen render path. Cheap: one
/// `Copy` snapshot behind a `Cell` plus a fallback counter, no allocation and no
/// clock, so the transition table is fully deterministic and an idle session
/// pays nothing.
#[derive(Debug, Default)]
pub(crate) struct LastGoodLayout {
    /// The most recent *valid* geometry, keyed by the size it was painted at.
    /// `None` until the first valid frame commits.
    good: Cell<Option<LayoutGeometry>>,
    /// How many times the store substituted the good snapshot for a degenerate
    /// frame this session. A bounded counter for the diagnostics line; never
    /// resets.
    fallback_count: Cell<u64>,
}

impl LastGoodLayout {
    /// Decide what geometry the renderer should paint this frame, and update the
    /// last-known-good snapshot.
    ///
    /// - If `current` is valid: record it as the new good snapshot and return
    ///   [`LayoutResolution::Use`].
    /// - If `current` is degenerate AND a good snapshot exists at the *same*
    ///   size: bump the fallback counter and return
    ///   [`LayoutResolution::Fallback`] carrying the good snapshot.
    /// - If `current` is degenerate but no same-size good snapshot exists:
    ///   return [`LayoutResolution::Use`] of `current` (best-effort; nothing
    ///   better to show). The degenerate frame is *not* recorded as good.
    ///
    /// Takes `&self` (interior `Cell` mutability) so the renderer can call it
    /// from the `&TuiApp` paint path without a borrow upgrade.
    pub(crate) fn resolve(&self, current: LayoutGeometry) -> LayoutResolution {
        if !current.is_degenerate() {
            self.good.set(Some(current));
            return LayoutResolution::Use(current);
        }
        // Degenerate frame: substitute the last good snapshot only if it was
        // taken at the exact size of this frame, so a resize never paints stale
        // geometry. Leave the good snapshot untouched (the degenerate frame is
        // never promoted to "good").
        if let Some(good) = self.good.get()
            && good.width == current.width
            && good.height == current.height
        {
            self.fallback_count
                .set(self.fallback_count.get().wrapping_add(1));
            return LayoutResolution::Fallback(good);
        }
        LayoutResolution::Use(current)
    }

    /// The current last-known-good snapshot, if any. The render path reads the
    /// good size through the diagnostics line; this typed accessor is the
    /// test/diagnostics surface.
    #[allow(dead_code)]
    pub(crate) fn good(&self) -> Option<LayoutGeometry> {
        self.good.get()
    }

    /// Total times the store substituted the good snapshot this session. Surfaced
    /// through `diagnostics_line`; exposed directly for the unit tests.
    #[allow(dead_code)]
    pub(crate) fn fallback_count(&self) -> u64 {
        self.fallback_count.get()
    }

    /// One-line, allocation-light diagnostics for the hidden HUD: whether a good
    /// snapshot is held, its size, and the running fallback count. Built only
    /// when the diagnostics overlay is on, so a normal session never formats it.
    pub(crate) fn diagnostics_line(&self) -> String {
        match self.good.get() {
            Some(good) => format!(
                "layout-fallback: good={}x{} falls={}",
                good.width,
                good.height,
                self.fallback_count.get()
            ),
            None => format!(
                "layout-fallback: good=none falls={}",
                self.fallback_count.get()
            ),
        }
    }
}

#[cfg(test)]
#[path = "layout_fallback_tests.rs"]
mod tests;
