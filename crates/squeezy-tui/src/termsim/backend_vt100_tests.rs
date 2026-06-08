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
    // Fixed grid: the logical row equals the clamped row and is populated.
    assert_eq!(grid.logical_cursor_row, 1);
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
