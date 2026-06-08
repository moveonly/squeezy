//! Logical scroll model and scrollbar geometry for the transcript view.
//!
//! This module is a **staged, additive** introduction of a `usize`-backed
//! scroll model (plan Phase 4, "MOVE 5"). It defines the new types but does
//! **not** widen any existing `u16` field in `lib.rs` yet — that migration
//! happens in a later move. Nothing here is wired into the renderer.
//!
//! The semantics deliberately mirror the existing helpers in `lib.rs` so the
//! eventual swap is behavior-preserving:
//!
//! * [`ScrollState::offset`] replicates `transcript_scroll_offset` — the
//!   "distance scrolled up from the tail" (`from_bottom`) is converted into an
//!   absolute top-line offset, clamped to the available scroll range.
//! * [`scrollbar_geometry`] replicates `transcript_overlay_scrollbar_geometry`
//!   — thumb length is proportional to the viewport/content ratio and the thumb
//!   is positioned along its travel by the current scroll offset.
//!
//! Everything is computed in `usize` to remove the historical `u16` ceiling on
//! row counts. Conversion to `u16` (for ratatui geometry) is funneled through
//! the single [`to_u16_clamped`] helper.
//!
//! Wiring status (parallelization-plan Phase 4 / MOVE 5): now that the
//! transcript scroll field is widened to `usize`, [`to_u16_clamped`] is used in
//! production at the ratatui boundary, so the module no longer needs a blanket
//! `allow(dead_code)`. The logical [`ScrollState`] and [`scrollbar_geometry`]
//! are still test-only until the renderer drives the main view through them; the
//! few remaining not-yet-wired items carry a targeted `#[allow(dead_code)]` at
//! their definition so everything else participates in dead-code analysis.

/// Saturating conversion from `usize` to `u16`.
///
/// Values at or above [`u16::MAX`] clamp to [`u16::MAX`]. This is the *only*
/// place the new `usize` model narrows back to the `u16` world that ratatui
/// geometry uses, so the truncation policy lives in exactly one spot.
#[inline]
#[must_use]
pub(crate) fn to_u16_clamped(value: usize) -> u16 {
    value.min(u16::MAX as usize) as u16
}

/// Logical scroll position for an append-only transcript.
///
/// The position is stored as `from_bottom`: the number of lines the viewport's
/// *top* has been scrolled up away from the position that would show the very
/// last line of content. `from_bottom == 0` therefore means "showing the tail".
///
/// `follow_tail` records intent: while following, the view should stay pinned to
/// the bottom as new content arrives (mirroring how the renderer keeps
/// `from_bottom` at 0). Scrolling up unpins; pinning to bottom re-pins.
///
/// Not-yet-wired (Phase 4 integration); test-only today.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScrollState {
    /// Lines scrolled up from the tail. `0` == pinned to the last line.
    from_bottom: usize,
    /// Whether the view should track the tail as content grows.
    follow_tail: bool,
}

impl Default for ScrollState {
    /// A fresh view follows the tail at `from_bottom == 0`.
    fn default() -> Self {
        Self {
            from_bottom: 0,
            follow_tail: true,
        }
    }
}

// Not-yet-wired (Phase 4 integration): the logical scroll API is test-only
// until the renderer drives the main view through it.
#[allow(dead_code)]
impl ScrollState {
    /// Construct a state pinned to the bottom (following the tail).
    #[must_use]
    pub(crate) fn pinned() -> Self {
        Self::default()
    }

    /// The raw `from_bottom` distance (unclamped, as stored).
    // This is a field getter named after the `from_bottom` field, not a
    // `from_*` converter; the conventional "no `self`" rule does not apply.
    #[allow(clippy::wrong_self_convention)]
    #[must_use]
    pub(crate) fn from_bottom(&self) -> usize {
        self.from_bottom
    }

    /// Whether the view is currently following the tail.
    #[must_use]
    pub(crate) fn is_following(&self) -> bool {
        self.follow_tail
    }

    /// The maximum `from_bottom` that still shows content, i.e. the number of
    /// lines that can scroll off the top before the first line is at the top.
    ///
    /// Equivalent to `transcript_scroll_offset`'s `max_scroll`
    /// (`line_count - viewport`), saturating at 0 when content fits.
    #[must_use]
    fn max_scroll(line_count: usize, viewport_h: usize) -> usize {
        line_count.saturating_sub(viewport_h)
    }

    /// Absolute top-line offset for rendering, clamped to the valid range.
    ///
    /// Mirrors `transcript_scroll_offset(line_count, area_height, from_bottom)`:
    /// `max_scroll = line_count - viewport`, returning
    /// `max_scroll - from_bottom` (saturating). When `from_bottom == 0` this is
    /// `max_scroll` — the tail. When `from_bottom >= max_scroll` this is `0` —
    /// the top.
    #[must_use]
    pub(crate) fn offset(&self, line_count: usize, viewport_h: usize) -> usize {
        let max_scroll = Self::max_scroll(line_count, viewport_h);
        max_scroll.saturating_sub(self.from_bottom)
    }

    /// Clamp the stored `from_bottom` so it never exceeds the current
    /// `max_scroll`. Returns `true` if the value changed.
    ///
    /// Call this after the content length or viewport changes. If the state is
    /// following the tail, it is re-pinned to `0` regardless.
    pub(crate) fn clamp(&mut self, line_count: usize, viewport_h: usize) -> bool {
        let before = self.from_bottom;
        if self.follow_tail {
            self.from_bottom = 0;
        } else {
            let max_scroll = Self::max_scroll(line_count, viewport_h);
            self.from_bottom = self.from_bottom.min(max_scroll);
        }
        self.from_bottom != before
    }

    /// Scroll by `delta_lines`: positive scrolls **up** (away from the tail,
    /// increasing `from_bottom`), negative scrolls **down** (toward the tail).
    ///
    /// The result is clamped to `[0, max_scroll]`. Reaching `0` re-pins to the
    /// tail (`follow_tail = true`); any upward movement unpins.
    pub(crate) fn scroll_by(&mut self, delta_lines: isize, line_count: usize, viewport_h: usize) {
        let max_scroll = Self::max_scroll(line_count, viewport_h);
        let next = if delta_lines >= 0 {
            self.from_bottom
                .saturating_add(delta_lines as usize)
                .min(max_scroll)
        } else {
            // delta_lines is negative; magnitude moves us toward the tail.
            let down = delta_lines.unsigned_abs();
            self.from_bottom.saturating_sub(down)
        };
        self.from_bottom = next;
        self.follow_tail = next == 0;
    }

    /// Pin the view to the bottom: `from_bottom = 0`, `follow_tail = true`.
    pub(crate) fn pin_to_bottom(&mut self) {
        self.from_bottom = 0;
        self.follow_tail = true;
    }
}

/// Pure scrollbar geometry for a vertical track of `viewport_h` rows showing
/// `total_rows` rows of content scrolled up by `from_bottom` lines.
///
/// Returns `None` when no scrollbar should be drawn: a zero-height track, or
/// content that fits entirely within the viewport (`total_rows <= viewport_h`).
///
/// Mirrors `transcript_overlay_scrollbar_geometry`. Note that function takes a
/// resolved *top-line* `scroll` offset, whereas this takes `from_bottom`; the
/// two coordinate systems produce the same thumb position because
/// `scroll = max_scroll - from_bottom` and the thumb travels from top (scroll 0)
/// to bottom (scroll == max_scroll). Here `from_bottom == 0` (the tail) places
/// the thumb at the bottom of its travel.
///
/// Not-yet-wired (Phase 4 integration); test-only today.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScrollbarGeometry {
    /// Offset of the thumb's top edge from the top of the track, in rows.
    pub(crate) thumb_offset: usize,
    /// Length of the thumb, in rows (always `>= 1` when present).
    pub(crate) thumb_len: usize,
}

/// Compute [`ScrollbarGeometry`] for the given content/viewport/scroll.
///
/// See [`ScrollbarGeometry`] for the `None` cases. The thumb length is the
/// proportional `track * track / content`, clamped to `[1, track]`, and the
/// thumb top is `scroll * travel / max_scroll` where `scroll` is the resolved
/// top-line offset (`max_scroll - from_bottom`).
///
/// Not-yet-wired (Phase 4 integration); test-only today.
#[allow(dead_code)]
#[must_use]
pub(crate) fn scrollbar_geometry(
    total_rows: usize,
    viewport_h: usize,
    from_bottom: usize,
) -> Option<ScrollbarGeometry> {
    let track_height = viewport_h;
    if track_height == 0 || total_rows <= track_height {
        return None;
    }
    let max_scroll = total_rows.saturating_sub(track_height);
    if max_scroll == 0 {
        return None;
    }
    let thumb_len = ((track_height * track_height) / total_rows).clamp(1, track_height);
    let travel = track_height.saturating_sub(thumb_len);
    // Convert the "distance from tail" into the renderer's top-line scroll, the
    // same coordinate `transcript_overlay_scrollbar_geometry` consumes.
    let scroll = max_scroll.saturating_sub(from_bottom.min(max_scroll));
    let thumb_offset = if travel == 0 {
        0
    } else {
        // `scroll * travel` can exceed usize for very large row counts (the
        // whole point of the migration), so widen the product to u128. The
        // quotient is bounded by `travel < track_height`, so the narrowing
        // back to usize is always lossless.
        ((scroll as u128 * travel as u128) / max_scroll as u128) as usize
    };
    Some(ScrollbarGeometry {
        thumb_offset,
        thumb_len,
    })
}

#[cfg(test)]
#[path = "scroll_tests.rs"]
mod tests;
