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
//! TODO(parallelization-plan Phase 4 / MOVE 5): this module is compiled but not
//! yet wired into the renderer (the `u16` scroll field is migrated in a later
//! move). The module-level `allow(dead_code)` below keeps warning-clean builds
//! green until a caller exists; remove it when the scroll field is widened.
#![allow(dead_code)]

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
mod tests {
    use super::*;

    // ---- to_u16_clamped -------------------------------------------------

    #[test]
    fn to_u16_clamped_passes_small_values() {
        assert_eq!(to_u16_clamped(0), 0);
        assert_eq!(to_u16_clamped(42), 42);
        assert_eq!(to_u16_clamped(u16::MAX as usize - 1), u16::MAX - 1);
    }

    #[test]
    fn to_u16_clamped_saturates_at_max() {
        assert_eq!(to_u16_clamped(u16::MAX as usize), u16::MAX);
        assert_eq!(to_u16_clamped(u16::MAX as usize + 1), u16::MAX);
        assert_eq!(to_u16_clamped(1_000_000), u16::MAX);
        assert_eq!(to_u16_clamped(usize::MAX), u16::MAX);
    }

    // ---- ScrollState::offset (mirrors transcript_scroll_offset) ----------

    /// Reference implementation: the exact logic of `transcript_scroll_offset`
    /// in lib.rs, used to cross-check the usize model over a u16 range.
    fn reference_offset(line_count: usize, area_height: u16, from_bottom: u16) -> u16 {
        let visible_lines = area_height as usize;
        let max_scroll = line_count.saturating_sub(visible_lines);
        max_scroll.saturating_sub(from_bottom as usize) as u16
    }

    #[test]
    fn offset_empty_buffer_is_zero() {
        let s = ScrollState::pinned();
        assert_eq!(s.offset(0, 0), 0);
        assert_eq!(s.offset(0, 24), 0);
    }

    #[test]
    fn offset_exact_fit_is_zero() {
        // Content exactly fills the viewport: nothing to scroll, offset is 0.
        let s = ScrollState::pinned();
        assert_eq!(s.offset(24, 24), 0);
        let mut up = ScrollState::pinned();
        up.scroll_by(5, 24, 24);
        assert_eq!(up.offset(24, 24), 0);
    }

    #[test]
    fn offset_overflow_tail_shows_bottom() {
        // 100 lines, 24-row viewport => max_scroll 76. Pinned (from_bottom 0)
        // shows the tail at offset 76.
        let s = ScrollState::pinned();
        assert_eq!(s.offset(100, 24), 76);
    }

    #[test]
    fn offset_scrolled_up_subtracts_from_max() {
        let mut s = ScrollState::pinned();
        s.scroll_by(10, 100, 24); // from_bottom = 10
        assert_eq!(s.offset(100, 24), 66);
    }

    #[test]
    fn offset_scrolled_past_top_clamps_to_zero() {
        let mut s = ScrollState::pinned();
        s.scroll_by(1000, 100, 24); // clamped to max_scroll = 76
        assert_eq!(s.from_bottom(), 76);
        assert_eq!(s.offset(100, 24), 0);
    }

    #[test]
    fn offset_matches_reference_over_u16_range() {
        let cases = [
            (0usize, 0u16, 0u16),
            (0, 24, 0),
            (24, 24, 0),
            (24, 24, 5),
            (100, 24, 0),
            (100, 24, 10),
            (100, 24, 76),
            (100, 24, 1000),
            (65_000, 80, 0),
            (65_000, 80, 100),
            (1, 1, 0),
        ];
        for (line_count, area_h, from_bottom) in cases {
            let s = ScrollState {
                from_bottom: from_bottom as usize,
                follow_tail: from_bottom == 0,
            };
            let got = to_u16_clamped(s.offset(line_count, area_h as usize));
            let want = reference_offset(line_count, area_h, from_bottom);
            assert_eq!(got, want, "case {line_count}/{area_h}/{from_bottom}");
        }
    }

    // ---- clamp ----------------------------------------------------------

    #[test]
    fn clamp_following_re_pins_to_zero() {
        let mut s = ScrollState {
            from_bottom: 50,
            follow_tail: true,
        };
        let changed = s.clamp(100, 24);
        assert!(changed);
        assert_eq!(s.from_bottom(), 0);
        assert!(s.is_following());
    }

    #[test]
    fn clamp_caps_unpinned_to_max_scroll() {
        let mut s = ScrollState {
            from_bottom: 500,
            follow_tail: false,
        };
        // max_scroll = 100 - 24 = 76
        let changed = s.clamp(100, 24);
        assert!(changed);
        assert_eq!(s.from_bottom(), 76);
        assert!(!s.is_following());
    }

    #[test]
    fn clamp_noop_returns_false() {
        let mut s = ScrollState {
            from_bottom: 10,
            follow_tail: false,
        };
        let changed = s.clamp(100, 24);
        assert!(!changed);
        assert_eq!(s.from_bottom(), 10);
    }

    #[test]
    fn clamp_when_content_fits_drops_to_zero() {
        let mut s = ScrollState {
            from_bottom: 5,
            follow_tail: false,
        };
        // Content shrank to fit: max_scroll = 0.
        let changed = s.clamp(20, 24);
        assert!(changed);
        assert_eq!(s.from_bottom(), 0);
    }

    // ---- scroll_by / follow-tail pin & unpin ----------------------------

    #[test]
    fn scroll_up_unpins() {
        let mut s = ScrollState::pinned();
        assert!(s.is_following());
        s.scroll_by(3, 100, 24);
        assert_eq!(s.from_bottom(), 3);
        assert!(!s.is_following());
    }

    #[test]
    fn scroll_down_to_tail_re_pins() {
        let mut s = ScrollState::pinned();
        s.scroll_by(10, 100, 24);
        assert!(!s.is_following());
        s.scroll_by(-10, 100, 24);
        assert_eq!(s.from_bottom(), 0);
        assert!(s.is_following());
    }

    #[test]
    fn scroll_down_past_tail_saturates_and_pins() {
        let mut s = ScrollState::pinned();
        s.scroll_by(5, 100, 24);
        s.scroll_by(-100, 100, 24);
        assert_eq!(s.from_bottom(), 0);
        assert!(s.is_following());
    }

    #[test]
    fn scroll_up_past_top_clamps_to_max_scroll() {
        let mut s = ScrollState::pinned();
        s.scroll_by(10_000, 100, 24);
        assert_eq!(s.from_bottom(), 76); // max_scroll
        assert!(!s.is_following());
    }

    #[test]
    fn scroll_partial_down_stays_unpinned() {
        let mut s = ScrollState::pinned();
        s.scroll_by(10, 100, 24);
        s.scroll_by(-4, 100, 24);
        assert_eq!(s.from_bottom(), 6);
        assert!(!s.is_following());
    }

    #[test]
    fn pin_to_bottom_resets() {
        let mut s = ScrollState {
            from_bottom: 40,
            follow_tail: false,
        };
        s.pin_to_bottom();
        assert_eq!(s.from_bottom(), 0);
        assert!(s.is_following());
    }

    #[test]
    fn scroll_when_content_fits_is_noop() {
        let mut s = ScrollState::pinned();
        s.scroll_by(50, 10, 24); // max_scroll = 0
        assert_eq!(s.from_bottom(), 0);
        assert!(s.is_following());
    }

    // ---- scrollbar_geometry (mirrors overlay geometry) ------------------

    /// Reference: the exact thumb math from
    /// `transcript_overlay_scrollbar_geometry`, taking a top-line `scroll`.
    fn reference_geometry(
        content_len: usize,
        viewport_height: u16,
        scroll: usize,
    ) -> Option<(u16, u16)> {
        let track_height = usize::from(viewport_height);
        if track_height == 0 || content_len <= track_height {
            return None;
        }
        let max_scroll = content_len.saturating_sub(track_height);
        if max_scroll == 0 {
            return None;
        }
        let thumb_height = ((track_height * track_height) / content_len).clamp(1, track_height);
        let travel = track_height.saturating_sub(thumb_height);
        let scroll = scroll.min(max_scroll);
        let thumb_top = if travel == 0 {
            0
        } else {
            scroll * travel / max_scroll
        };
        Some((thumb_top as u16, thumb_height as u16))
    }

    #[test]
    fn geometry_none_for_empty_buffer() {
        assert_eq!(scrollbar_geometry(0, 24, 0), None);
        assert_eq!(scrollbar_geometry(0, 0, 0), None);
    }

    #[test]
    fn geometry_none_when_content_fits() {
        assert_eq!(scrollbar_geometry(24, 24, 0), None);
        assert_eq!(scrollbar_geometry(10, 24, 0), None);
    }

    #[test]
    fn geometry_none_for_zero_height_track() {
        assert_eq!(scrollbar_geometry(100, 0, 0), None);
    }

    #[test]
    fn geometry_thumb_len_is_proportional() {
        // 100 rows, 24-row track => 24*24/100 = 5.
        let g = scrollbar_geometry(100, 24, 0).unwrap();
        assert_eq!(g.thumb_len, 5);
    }

    #[test]
    fn geometry_thumb_len_clamped_to_minimum_one() {
        // Huge content, small track => proportional thumb rounds to 0, clamped to 1.
        let g = scrollbar_geometry(1_000_000, 2, 0).unwrap();
        assert_eq!(g.thumb_len, 1);
    }

    #[test]
    fn geometry_tail_places_thumb_at_bottom_of_travel() {
        // from_bottom == 0 (tail) => scroll == max_scroll => thumb at end of travel.
        let g = scrollbar_geometry(100, 24, 0).unwrap();
        let travel = 24 - g.thumb_len;
        assert_eq!(g.thumb_offset, travel);
    }

    #[test]
    fn geometry_top_places_thumb_at_offset_zero() {
        // Scrolled fully up: from_bottom == max_scroll => scroll 0 => thumb at top.
        let max_scroll = 100 - 24;
        let g = scrollbar_geometry(100, 24, max_scroll).unwrap();
        assert_eq!(g.thumb_offset, 0);
    }

    #[test]
    fn geometry_from_bottom_beyond_max_clamps() {
        let beyond = scrollbar_geometry(100, 24, 10_000).unwrap();
        let at_top = scrollbar_geometry(100, 24, 100 - 24).unwrap();
        assert_eq!(beyond, at_top);
    }

    #[test]
    fn geometry_matches_reference_via_offset() {
        // For each from_bottom, our geometry must equal the reference geometry
        // fed the equivalent top-line scroll (max_scroll - from_bottom).
        let content = 100usize;
        let viewport = 24u16;
        let max_scroll = content - viewport as usize;
        for from_bottom in [0usize, 1, 10, 40, 76, 200] {
            let scroll = max_scroll.saturating_sub(from_bottom.min(max_scroll));
            let got = scrollbar_geometry(content, viewport as usize, from_bottom)
                .map(|g| (to_u16_clamped(g.thumb_offset), to_u16_clamped(g.thumb_len)));
            let want = reference_geometry(content, viewport, scroll);
            assert_eq!(got, want, "from_bottom={from_bottom}");
        }
    }

    // ---- >65k rows (the reason for the usize migration) -----------------

    #[test]
    fn offset_beyond_u16_range() {
        let line_count = 100_000usize;
        let viewport = 50usize;
        let s = ScrollState::pinned();
        // Tail offset = 100_000 - 50 = 99_950, which exceeds u16::MAX.
        assert_eq!(s.offset(line_count, viewport), 99_950);
        assert!(s.offset(line_count, viewport) > u16::MAX as usize);
    }

    #[test]
    fn scroll_by_beyond_u16_range() {
        let line_count = 100_000usize;
        let viewport = 50usize;
        let mut s = ScrollState::pinned();
        s.scroll_by(70_000, line_count, viewport);
        assert_eq!(s.from_bottom(), 70_000);
        assert!(s.from_bottom() > u16::MAX as usize);
        // Offset = max_scroll(99_950) - 70_000 = 29_950.
        assert_eq!(s.offset(line_count, viewport), 29_950);
    }

    #[test]
    fn geometry_beyond_u16_rows_thumb_clamps_to_one() {
        let g = scrollbar_geometry(100_000, 40, 0).unwrap();
        // 40*40/100_000 = 0 -> clamped to 1.
        assert_eq!(g.thumb_len, 1);
        // Tail: thumb at bottom of travel.
        assert_eq!(g.thumb_offset, 40 - 1);
    }

    #[test]
    fn geometry_thumb_offset_can_exceed_u16_only_after_clamp() {
        // thumb_offset is bounded by the track height, so it never exceeds u16,
        // even with enormous content. Sanity check that invariant.
        let g = scrollbar_geometry(usize::MAX / 2, 30_000, 0).unwrap();
        assert!(g.thumb_offset <= 30_000);
        assert!(g.thumb_len >= 1 && g.thumb_len <= 30_000);
    }
}
