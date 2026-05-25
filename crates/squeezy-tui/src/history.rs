//! History-cell rendering trait + per-cell line cache.
//!
//! Squeezy renders the transcript as a single growing `Vec<Line<'static>>`.
//! Anything that wants to participate in the transcript layout pipeline
//! implements [`HistoryCell`] and gets:
//!   * width-keyed line caching for free (via [`RenderCache`])
//!   * a uniform `desired_height` / `render` shape that other widgets
//!     (working row, streaming tail) can compose with.
//!
//! Today the transcript is `Vec<TranscriptEntry>`, not `Vec<Box<dyn
//! HistoryCell>>`. That's deliberate: the existing rendering already
//! does per-entry layout, so the migration value comes from caching and
//! from giving the working row + streaming tail a common shape. Future
//! work can shift the whole transcript to `Vec<Box<dyn HistoryCell>>`
//! without touching this file.

#![allow(dead_code)]

use ratatui::text::Line;

/// Anything that renders a contiguous block of lines into the transcript.
pub(crate) trait HistoryCell {
    /// Number of rows this cell needs at `width`. Implementations should
    /// be cheap on cache hits.
    fn desired_height(&self, width: u16) -> u16;

    /// Produce the lines for this cell at `width`. Implementations may
    /// return owned lines (when streaming/animating) or cached clones.
    fn render(&self, width: u16) -> Vec<Line<'static>>;

    /// `true` if this cell repaints between frames even without source
    /// changes (e.g. shimmer, streaming tail). Used by the redraw loop
    /// to schedule a follow-up frame.
    fn is_animating(&self) -> bool {
        false
    }

    /// Clear any cached layout. Called when source data changes.
    fn invalidate(&mut self) {}
}

/// Width-keyed line cache shared by `HistoryCell` implementors.
///
/// Cache is invalidated whenever the requested width differs from the
/// stored width, or `invalidate()` is called explicitly. Re-computation
/// is delegated to a closure so each cell type controls its own layout.
#[derive(Debug, Default, Clone)]
pub(crate) struct RenderCache {
    cached_width: Option<u16>,
    lines: Vec<Line<'static>>,
}

impl RenderCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns cached lines, recomputing if the width changed.
    pub(crate) fn ensure<F>(&mut self, width: u16, compute: F) -> &[Line<'static>]
    where
        F: FnOnce(u16) -> Vec<Line<'static>>,
    {
        if self.cached_width != Some(width) {
            self.lines = compute(width);
            self.cached_width = Some(width);
        }
        &self.lines
    }

    /// Returns the cached height for `width`, computing on miss.
    pub(crate) fn height<F>(&mut self, width: u16, compute: F) -> u16
    where
        F: FnOnce(u16) -> Vec<Line<'static>>,
    {
        self.ensure(width, compute).len() as u16
    }

    /// Drop any cached lines so the next `ensure` recomputes.
    pub(crate) fn invalidate(&mut self) {
        self.cached_width = None;
        self.lines.clear();
    }

    #[cfg(test)]
    pub(crate) fn is_warm(&self) -> bool {
        self.cached_width.is_some()
    }
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
