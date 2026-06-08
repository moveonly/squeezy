//! The [`Emulator`] trait and shared replay helpers.
//!
//! Per §7.1 of the plan, the only trustworthy replay splits the captured
//! stream on the DEC-2026 synchronized-update BEGIN marker, recovers each
//! frame's width from the `☽`-divider dash count, and `resize(w, h)`s the
//! emulator per frame before feeding that frame's bytes. Both Rust backends
//! and the future xterm.js bridge call the helpers here so there is a single
//! replay implementation.
//!
//! This is scaffolding: the helpers are stubs that compile against the type
//! shapes but do not yet implement the real frame-splitting logic.

use super::types::{CaptureLog, EmulatorProfile, FrameMark, Grid};

/// A terminal emulator backend that can replay a captured ANSI byte stream
/// into a normalized [`Grid`], and describe its own behavior via an
/// [`EmulatorProfile`].
pub(crate) trait Emulator {
    /// Replay the whole capture log and return the final reconstructed grid.
    fn replay(&self, log: &CaptureLog) -> Grid;

    /// Static description of this backend's reflow / cursor / glyph-width
    /// behavior, used to label matrix output.
    fn profile(&self) -> EmulatorProfile;
}

/// One named emulator leg in the matrix registry.
pub(crate) struct NamedBackend {
    /// Stable identifier used in matrix output and snapshot names
    /// (`"vt100"`, `"alacritty"`).
    pub name: &'static str,
    /// The backend itself, boxed so legs of different concrete types live in
    /// one homogeneous registry.
    pub emulator: Box<dyn Emulator>,
}

/// The always-on Rust emulator legs, gated by `term-matrix`. vt100 (fixed grid,
/// no reflow) is always present; alacritty (reflow + cursor tracking) is
/// included because it is implemented. The xterm.js leg is out-of-process and
/// is not part of this in-process registry (it consumes an exported
/// [`CaptureLog`] JSON instead).
///
/// Returned as owned boxes so callers can iterate `&dyn Emulator` uniformly and
/// name each leg in failure messages / snapshot files.
pub(crate) fn all_backends() -> Vec<NamedBackend> {
    use super::backend_alacritty::AlacrittyEmulator;
    use super::backend_vt100::Vt100Emulator;

    vec![
        NamedBackend {
            name: "vt100",
            emulator: Box::new(Vt100Emulator),
        },
        NamedBackend {
            name: "alacritty",
            emulator: Box::new(AlacrittyEmulator),
        },
    ]
}

/// Split a [`CaptureLog`] into per-frame byte slices using the recorded
/// [`FrameMark`] offsets. Frame *i* is `bytes[start..mark.byte_offset]`,
/// where `start` is the previous mark's offset (or 0 for the first frame).
///
/// Offsets are clamped to the byte buffer and to a monotonically
/// non-decreasing cursor so a malformed/truncated capture yields empty slices
/// rather than panicking on an out-of-range or backwards range.
pub(crate) fn split_frames(log: &CaptureLog) -> Vec<FrameBytes<'_>> {
    let len = log.bytes.len();
    let mut start = 0usize;
    let mut out = Vec::with_capacity(log.frames.len());
    for mark in &log.frames {
        let end = mark.byte_offset.min(len).max(start);
        out.push(FrameBytes {
            bytes: &log.bytes[start..end],
            mark: *mark,
        });
        start = end;
    }
    out
}

/// One frame's worth of bytes plus the size in effect for that frame.
pub(crate) struct FrameBytes<'a> {
    /// The raw bytes emitted for this frame.
    pub bytes: &'a [u8],
    /// The frame's marker (offset + width/height).
    pub mark: FrameMark,
}

/// Recover a frame's logical width from its `☽`-divider dash count, the
/// per-frame-width reconstruction §7.1 proves is the only reliable signal.
///
/// Scaffolding stub: returns `None` until the dash-count parser lands.
pub(crate) fn width_from_divider(_frame: &[u8]) -> Option<u16> {
    None
}
