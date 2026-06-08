//! The `vt100` emulator leg: a fixed grid with no reflow.
//!
//! vt100 models a classic fixed-size screen — on resize it clips/pads rather
//! than rewrapping, which makes it the conservative oracle for "what survives
//! without reflow". Always-on Rust leg of the matrix.
//!
//! # Replay model
//!
//! We drive a single [`vt100::Parser`] across the whole capture. The stream is
//! sliced into per-frame byte ranges via [`super::emulator::split_frames`]
//! (each slice is `bytes[prev_offset..mark.byte_offset]`). Before feeding a
//! frame's bytes we apply that frame's `(w, h)` with [`vt100::Screen::set_size`]
//! — vt100's `set_size` resizes the live grid in place and does **not** reflow
//! wrapped lines, so a width change clips/pads columns and a height change
//! clips/pads rows. After the final frame we read the screen back into a
//! [`Grid`].
//!
//! # Reading the screen
//!
//! * Viewport rows come from [`vt100::Screen::rows`]`(0, cols)`, which yields
//!   one plain-text `String` per visible row (no formatting, no trailing
//!   newline). When the alternate screen is active these *are* the alt-screen
//!   rows, so we mirror them into [`Grid::alt_screen`] and report
//!   [`vt100::Screen::alternate_screen`].
//! * The cursor comes from [`vt100::Screen::cursor_position`], which returns
//!   `(row, col)`; [`Grid::cursor`] is `(col, row)`, so we swap.
//! * vt100 keeps scrollback in a separate buffer that `rows()` does not expose
//!   without scrolling the viewport, so [`Grid::scrollback`] / [`Grid::base_y`]
//!   stay empty/zero for this leg — the fixed grid only reconstructs the live
//!   viewport, which is exactly the §8.5 surface the assertions inspect.

use super::emulator::{Emulator, split_frames};
use super::types::{CaptureLog, CursorTracking, EmulatorProfile, Grid};

/// Fixed-grid emulator backed by the `vt100` crate.
#[derive(Debug, Default)]
pub(crate) struct Vt100Emulator;

impl Emulator for Vt100Emulator {
    fn replay(&self, log: &CaptureLog) -> Grid {
        let frames = split_frames(log);

        // Size the parser to the first frame so the very first paint lands on a
        // correctly-sized grid; fall back to a classic 80x24 when the capture
        // recorded no frames (empty log).
        let (init_rows, init_cols) = frames
            .first()
            .map_or((24, 80), |f| (f.mark.h.max(1), f.mark.w.max(1)));
        let mut parser = vt100::Parser::new(init_rows, init_cols, 0);

        for frame in &frames {
            // Apply this frame's size BEFORE feeding its bytes. vt100's
            // set_size is a fixed-grid resize: it clips/pads rather than
            // reflowing, which is the no-reflow oracle behavior we want.
            parser
                .screen_mut()
                .set_size(frame.mark.h.max(1), frame.mark.w.max(1));
            parser.process(frame.bytes);
        }

        screen_to_grid(parser.screen())
    }

    fn profile(&self) -> EmulatorProfile {
        EmulatorProfile {
            // vt100 is a fixed grid: a resize clips/pads, it never rewraps.
            reflows: false,
            // Cursor-vs-wrap tracking is meaningless without reflow, so this
            // leg reports NotApplicable rather than claiming a policy.
            cursor_tracking: CursorTracking::NotApplicable,
            // vt100 measures glyph width via `unicode-width`, which treats
            // ambiguous-width glyphs as narrow.
            ambiguous_glyph_width: 1,
        }
    }
}

/// Read a settled [`vt100::Screen`] into the normalized [`Grid`].
fn screen_to_grid(screen: &vt100::Screen) -> Grid {
    let (rows, cols) = screen.size();
    // `rows(0, cols)` yields one plain-text String per visible row, top to
    // bottom, with no trailing newline.
    let viewport: Vec<String> = screen.rows(0, cols).collect();

    // When the alternate screen is active the visible rows ARE the alt-screen
    // overlay, so surface them in `alt_screen` too (the assertions look there
    // while an overlay is up).
    let alt_screen = if screen.alternate_screen() {
        viewport.clone()
    } else {
        Vec::new()
    };

    // vt100 reports the cursor as (row, col); Grid stores it as (col, row).
    let (cursor_row, cursor_col) = screen.cursor_position();
    debug_assert!(cursor_row <= rows, "cursor row escaped the grid");

    Grid {
        viewport,
        alt_screen,
        // The fixed-grid leg only reconstructs the live viewport; vt100's
        // separate scrollback buffer is not part of the §8.5 surface.
        scrollback: Vec::new(),
        cursor: (cursor_col, cursor_row),
        base_y: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::termsim::types::{CaptureLog, FrameMark};

    /// Build a single-frame [`CaptureLog`] from raw ANSI bytes at size `(w, h)`.
    fn one_frame(bytes: &[u8], w: u16, h: u16) -> CaptureLog {
        CaptureLog {
            bytes: bytes.to_vec(),
            frames: vec![FrameMark {
                byte_offset: bytes.len(),
                w,
                h,
            }],
        }
    }

    #[test]
    fn replays_known_ansi_into_fixed_grid() {
        // A tiny, fully-specified sequence on a 6x3 grid:
        //   "AB"            -> row 0 text = "AB"
        //   CR LF           -> move to start of row 1
        //   ESC[31m "CD"    -> row 1 text = "CD" (color is dropped in plain text)
        // Cursor ends just past "CD" on row 1: (col 2, row 1).
        //
        // vt100's `rows()` returns plain text with trailing blank cells
        // trimmed, so the empty row 2 reads as "" and the populated rows are
        // NOT padded to the 6-column width — that trimming is the emulator's
        // own contract and the grid reflects it faithfully.
        let bytes = b"AB\r\n\x1b[31mCD";
        let log = one_frame(bytes, 6, 3);

        let grid = Vt100Emulator.replay(&log);

        assert_eq!(
            grid.viewport,
            vec!["AB".to_string(), "CD".to_string(), String::new()],
            "viewport rows are plain text with trailing blanks trimmed",
        );
        // cursor_position() is (row, col); Grid stores (col, row).
        assert_eq!(grid.cursor, (2, 1), "cursor sits just past CD on row 1");
        assert!(grid.alt_screen.is_empty(), "no alt screen was entered");
        assert!(
            grid.scrollback.is_empty(),
            "fixed grid exposes no scrollback"
        );
        assert_eq!(grid.base_y, 0);
    }

    #[test]
    fn cursor_addressing_lands_on_the_fixed_grid() {
        // CSI H homes the cursor, then CSI 2;3H moves to row 2, col 3 (1-based)
        // and writes "X". On a 5x3 grid that is row 1, col 2 (0-based), so "X"
        // lands at column index 2 with two leading spaces; trailing blanks are
        // trimmed by vt100's `rows()`, so the row reads "  X". The cursor
        // advances to (col 3, row 1).
        let bytes = b"\x1b[H\x1b[2;3HX";
        let log = one_frame(bytes, 5, 3);

        let grid = Vt100Emulator.replay(&log);

        assert_eq!(grid.viewport[1], "  X".to_string());
        assert_eq!(grid.cursor, (3, 1));
    }

    #[test]
    fn last_frame_size_clips_the_fixed_grid_without_reflow() {
        // Two frames: the first paints "HELLO" on a wide 10x2 grid, the second
        // re-paints nothing but shrinks to 3x2. vt100's set_size clips columns
        // in place (no reflow), so the surviving row is the first 3 columns.
        let first = b"HELLO";
        let second: &[u8] = b""; // a settle frame that only carries a new size
        let mut bytes = Vec::new();
        bytes.extend_from_slice(first);
        let off1 = bytes.len();
        bytes.extend_from_slice(second);
        let off2 = bytes.len();
        let log = CaptureLog {
            bytes,
            frames: vec![
                FrameMark {
                    byte_offset: off1,
                    w: 10,
                    h: 2,
                },
                FrameMark {
                    byte_offset: off2,
                    w: 3,
                    h: 2,
                },
            ],
        };

        let grid = Vt100Emulator.replay(&log);

        assert_eq!(grid.viewport.len(), 2, "height clipped to the last frame");
        assert_eq!(
            grid.viewport[0],
            "HEL".to_string(),
            "width clipped to 3 columns with no reflow of LLO onto a new row",
        );
    }
}
