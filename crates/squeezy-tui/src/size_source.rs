//! Injectable terminal-size seam.
//!
//! Today the TUI reads the terminal dimensions by calling
//! `crossterm::terminal::size()` directly at a couple of render-path
//! sites (see `transcript_overlay_max_scroll` and the append-only
//! `paint_main`). That couples those code paths to a real terminal,
//! which makes them awkward to exercise from headless tests and from
//! `squeezy-eval` scenarios driving a `TestBackend`.
//!
//! This module introduces a small trait, [`SizeSource`], whose single
//! method returns the current terminal size, plus a production
//! implementation, [`RealSize`], that delegates to crossterm. A later
//! staged change will thread a `SizeSource` through the renderer (via a
//! field on the terminal guard) and swap the direct `terminal_size()`
//! calls over to it. Defining the seam here first keeps that follow-up
//! diff focused and the current `lib.rs` footprint at zero.
//!
//! ## Size convention
//!
//! Every method speaks the same tuple crossterm does:
//! `(columns, rows)` — i.e. `(width, height)`. The two existing call
//! sites already bind it that way:
//!
//! ```ignore
//! let (width, height) = terminal_size().ok()?;          // lib.rs ~2194
//! let (w, h)          = terminal_size().map_err(..)?;    // lib.rs ~18206
//! ```
//!
//! Implementations and the [`FixedSize`] test helper preserve that
//! `(cols, rows)` ordering so callers can substitute a `SizeSource`
//! without re-ordering the destructure.
//!
//! TODO(parallelization-plan): the seam is defined here but not yet threaded
//! through the renderer (that swaps the direct `terminal_size()` calls in a
//! later move). The module-level `allow(dead_code)` keeps warning-clean builds
//! green until the guard carries a `SizeSource`; remove it once wired.
#![allow(dead_code)]

use std::io;

/// Source of the current terminal dimensions, in `(columns, rows)`
/// order to mirror [`crossterm::terminal::size`].
///
/// The trait exists purely as an injection seam: production code uses
/// [`RealSize`], while tests use [`FixedSize`] to feed scripted
/// dimensions into the render path without owning a real terminal.
pub(crate) trait SizeSource {
    /// Returns the terminal size as `(cols, rows)`.
    ///
    /// Returns the same `io::Result` crossterm does so error handling
    /// at the call site is unchanged when the direct call is swapped
    /// for a `SizeSource`.
    fn size(&self) -> io::Result<(u16, u16)>;
}

/// Production [`SizeSource`] that queries the real terminal via
/// `crossterm::terminal::size()`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RealSize;

impl SizeSource for RealSize {
    fn size(&self) -> io::Result<(u16, u16)> {
        // `crossterm::terminal::size` already yields `(cols, rows)`.
        crossterm::terminal::size()
    }
}

/// Test [`SizeSource`] that always reports a fixed `(cols, rows)`
/// dimension, regardless of the actual terminal (or absence of one).
///
/// Used by unit tests and `squeezy-eval` scenarios to drive the render
/// path at a deterministic size. Constructed as `FixedSize(cols, rows)`
/// to match the `(width, height)` destructure used at the call sites.
#[cfg(any(test, feature = "testing"))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct FixedSize(pub u16, pub u16);

#[cfg(any(test, feature = "testing"))]
impl SizeSource for FixedSize {
    fn size(&self) -> io::Result<(u16, u16)> {
        Ok((self.0, self.1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_size_returns_scripted_dimensions() {
        let src = FixedSize(120, 40);
        assert_eq!(src.size().unwrap(), (120, 40));
    }

    #[test]
    fn fixed_size_preserves_cols_rows_order() {
        // First field is columns (width), second is rows (height);
        // this guards against an accidental swap when callers migrate
        // from `let (width, height) = ...`.
        let (cols, rows) = FixedSize(80, 24).size().unwrap();
        assert_eq!(cols, 80, "first field must be columns (width)");
        assert_eq!(rows, 24, "second field must be rows (height)");
    }

    #[test]
    fn fixed_size_allows_zero_dimensions() {
        // The append-only renderer treats a zero dimension as a no-op
        // frame; the seam must be able to reproduce that input exactly.
        assert_eq!(FixedSize(0, 0).size().unwrap(), (0, 0));
        assert_eq!(FixedSize(0, 30).size().unwrap(), (0, 30));
        assert_eq!(FixedSize(100, 0).size().unwrap(), (100, 0));
    }

    #[test]
    fn fixed_size_is_copy_and_repeatable() {
        let src = FixedSize(200, 50);
        let copy = src;
        // Copy semantics: the original is still usable after the copy.
        assert_eq!(src.size().unwrap(), (200, 50));
        assert_eq!(copy.size().unwrap(), (200, 50));
        // Repeated reads are stable (no internal scripting/consumption).
        assert_eq!(src.size().unwrap(), src.size().unwrap());
    }

    #[test]
    fn real_size_size_matches_crossterm_directly() {
        // In a headless test environment `crossterm::terminal::size`
        // may error (no tty); whichever way it resolves, `RealSize`
        // must agree byte-for-byte with the direct call it delegates to.
        match crossterm::terminal::size() {
            Ok(direct) => assert_eq!(RealSize.size().unwrap(), direct),
            Err(_) => assert!(RealSize.size().is_err()),
        }
    }
}
