use super::*;
use crate::termsim::types::{CaptureLog, FrameMark};

/// Build a `CaptureLog` whose whole byte stream is `bytes`, painted once at
/// `(w, h)`. Exercises the no-frame-splitter fallback path (the splitter is
/// a scaffold stub today), which still drives a real alacritty `Term`.
fn single_frame_log(bytes: &[u8], w: u16, h: u16) -> CaptureLog {
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
fn replays_plain_text_into_viewport() {
    let log = single_frame_log(b"hello world", 20, 5);
    let grid = AlacrittyEmulator.replay(&log);

    assert_eq!(grid.viewport.len(), 5, "viewport should have h rows");
    assert_eq!(grid.viewport[0], "hello world");
    // Cursor sits just past the last written column on the first row.
    assert_eq!(grid.cursor, (11, 0));
    // Plain text, no overlay: nothing on the alt screen.
    assert!(grid.alt_screen.is_empty());
}

#[test]
fn newlines_advance_rows_and_track_cursor() {
    // CR+LF between the two lines so the cursor returns to column 0.
    let log = single_frame_log(b"line one\r\nline two", 20, 5);
    let grid = AlacrittyEmulator.replay(&log);

    assert_eq!(grid.viewport[0], "line one");
    assert_eq!(grid.viewport[1], "line two");
    assert_eq!(grid.cursor, (8, 1));
    // The pre-clamp logical row is populated and, with the cursor in-grid,
    // matches the clamped row.
    assert_eq!(grid.logical_cursor_row, 1);
}

#[test]
fn reflows_wrapped_line_when_widened() {
    // 12 chars on a 6-wide screen wrap to two rows; widening to 20 should
    // rewrap them back onto a single row. This is the reflow behavior the
    // alacritty leg exists to model.
    let text = b"abcdefghijkl";
    let frames = vec![
        FrameMark {
            byte_offset: text.len(),
            w: 6,
            h: 4,
        },
        // Second frame: no new bytes, just the wider size. The fallback
        // path resizes to the *last* mark before replaying, so a 20-wide
        // last mark proves the wider grid holds the line unwrapped.
        FrameMark {
            byte_offset: text.len(),
            w: 20,
            h: 4,
        },
    ];
    let log = CaptureLog {
        bytes: text.to_vec(),
        frames,
    };
    let grid = AlacrittyEmulator.replay(&log);

    // After reflow to width 20 the whole word lives on row 0.
    assert_eq!(grid.viewport[0], "abcdefghijkl");
}

#[test]
fn profile_advertises_reflow_and_logical_tracking() {
    let p = AlacrittyEmulator.profile();
    assert!(p.reflows);
    assert_eq!(p.cursor_tracking, CursorTracking::TracksLogicalLine);
    assert_eq!(p.ambiguous_glyph_width, 1);
}
