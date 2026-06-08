//! Pure data shapes for the term-matrix framework.
//!
//! Deliberately backend-free: no `vt100`/`alacritty_terminal` types are in
//! scope here so these structs stay cheap and reusable across both Rust
//! emulator legs and the out-of-process xterm.js oracle. The capture types
//! ([`CaptureLog`]/[`FrameMark`]) mirror the append-only stream produced by
//! the real `paint_main` path (see the `§8` design notes), and [`Grid`] is
//! the normalized emulator output every backend reconstructs.

/// The concatenated real ANSI byte stream produced across a whole scenario
/// run, plus one marker per painted [`crate::termsim::scenario::Step::Frame`].
///
/// Frame *i*'s bytes are `bytes[frames[i-1].byte_offset .. frames[i].byte_offset]`
/// (frame 0 starts at offset 0), so the log is self-slicing per frame.
#[derive(Debug, Clone, Default)]
pub(crate) struct CaptureLog {
    /// Every byte the append-only path emitted across the whole run, in
    /// order, read verbatim out of the shared `Capture` sink.
    pub bytes: Vec<u8>,
    /// One mark per `Step::Frame`, in paint order.
    pub frames: Vec<FrameMark>,
}

/// Records where one painted frame ended in the [`CaptureLog::bytes`] stream
/// and the `FixedSize` in effect for that paint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameMark {
    /// Byte offset captured immediately AFTER this frame's paint flushed;
    /// the start of this frame is the previous mark's offset (or 0).
    pub byte_offset: usize,
    /// Terminal width (columns) in effect for this frame.
    pub w: u16,
    /// Terminal height (rows) in effect for this frame.
    pub h: u16,
}

/// Normalized emulator output: the reconstructed screen after replaying a
/// [`CaptureLog`] through a terminal emulator. Every backend produces one of
/// these so the §8.5 invariant assertions can run uniformly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Grid {
    /// Visible viewport rows (top to bottom), one `String` per row.
    pub viewport: Vec<String>,
    /// Alternate-screen rows, populated only while an overlay is active.
    pub alt_screen: Vec<String>,
    /// Committed scrollback above the viewport (oldest first).
    pub scrollback: Vec<String>,
    /// Cursor position as `(col, row)` within the viewport.
    pub cursor: (u16, u16),
    /// Row of the viewport top within the full (scrollback + viewport)
    /// space; the append-only renderer's notion of where "live" begins.
    pub base_y: u16,
}

/// Static description of how a given emulator backend behaves, so the matrix
/// output can name the differences between legs (notably the xterm.js cursor
/// drift that is the actual migration bug).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EmulatorProfile {
    /// Whether the emulator reflows wrapped lines on resize (alacritty does;
    /// vt100's fixed grid does not).
    pub reflows: bool,
    /// How the emulator tracks the cursor across wraps/resizes.
    pub cursor_tracking: CursorTracking,
    /// Display width policy for ambiguous-width glyphs (CJK / moon glyph):
    /// `1` (narrow) or `2` (wide).
    pub ambiguous_glyph_width: u8,
}

/// How an emulator keeps the cursor anchored across line wrapping and resize.
///
/// Modeled as an enum rather than a bool so the xterm.js drift profile — the
/// actual bug the matrix is built to catch — is nameable in matrix output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorTracking {
    /// Cursor stays anchored to its logical line regardless of wrapping
    /// (the well-behaved emulators).
    TracksLogicalLine,
    /// Cursor drifts downward by the number of below-fold wrapped rows
    /// (the xterm.js regression that produced the divider stack).
    DriftsByBelowWrapDelta,
    /// Cursor tracking is not meaningful for this backend.
    NotApplicable,
}
