//! Injectable terminal-size seam.
//!
//! The TUI used to read terminal dimensions by calling
//! `crossterm::terminal::size()` directly, which coupled those code paths to a
//! real terminal and made them awkward to exercise from headless tests and from
//! `squeezy-eval` scenarios driving a `TestBackend`.
//!
//! This module provides a small trait, [`SizeSource`], whose single method
//! returns the current terminal size, plus a production implementation,
//! [`RealSize`], that delegates to crossterm. The seam is now threaded through
//! the renderer: [`crate::TerminalGuard`] carries a `Box<dyn SizeSource>` (set to
//! [`RealSize`] in production, [`FixedSize`] in `for_capture_test`), and the
//! size-dependent paths — notably the clean-exit mirror width in
//! `TerminalGuard::finish_fullscreen` — read through it instead of calling
//! `crossterm::terminal::size()` directly, so they are driveable at a
//! deterministic size with no real TTY.
//!
//! ## Size convention
//!
//! Every method speaks the same tuple crossterm does:
//! `(columns, rows)` — i.e. `(width, height)`:
//!
//! ```ignore
//! let (w, _h) = self.size_source.size()?;    // finish_fullscreen mirror width
//! ```
//!
//! Implementations and the [`FixedSize`] test helper preserve that
//! `(cols, rows)` ordering so callers can substitute a `SizeSource`
//! without re-ordering the destructure.

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
/// Used by unit tests (`for_capture_test`) to drive the render path at a
/// deterministic size, and exposed under the `testing` feature for downstream
/// harnesses. Constructed as `FixedSize(cols, rows)` to match the
/// `(width, height)` destructure used at the call sites.
///
/// The only in-crate constructors are `#[cfg(test)]`, so under the `testing`
/// feature alone (no `test` cfg) this type is built but never instantiated
/// in-crate — hence the targeted dead-code allow there.
#[cfg(any(test, feature = "testing"))]
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct FixedSize(pub u16, pub u16);

#[cfg(any(test, feature = "testing"))]
impl SizeSource for FixedSize {
    fn size(&self) -> io::Result<(u16, u16)> {
        Ok((self.0, self.1))
    }
}

#[cfg(test)]
#[path = "size_source_tests.rs"]
mod tests;
