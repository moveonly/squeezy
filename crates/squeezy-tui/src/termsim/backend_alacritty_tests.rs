use super::*;
use crate::termsim::types::{CaptureLog, FrameMark};

/// Build a `CaptureLog` whose whole byte stream is `bytes`, painted once at
/// `(w, h)` — a single recorded frame, so `split_frames` yields one slice
/// covering the whole stream.
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
        // Second frame: no new bytes (same byte_offset), just the wider
        // size. The per-frame branch resizes to 20 before feeding this
        // (empty) frame, so the reflow proves the wider grid holds the line
        // unwrapped.
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
fn frameless_log_replays_whole_stream_at_fallback_size() {
    // A capture with bytes but NO frame marks exercises the `frames.is_empty()`
    // fallback branch: with no recorded size to resize to, the leg replays the
    // entire byte stream once at FALLBACK_SIZE (80x24) and still reconstructs a
    // real grid — unlike the vt100 leg, which leaves the frameless grid blank.
    let log = CaptureLog {
        bytes: b"frameless replay".to_vec(),
        frames: vec![],
    };

    let grid = AlacrittyEmulator.replay(&log);

    assert_eq!(grid.viewport.len(), 24, "fallback height is 24 rows");
    assert_eq!(
        grid.viewport[0], "frameless replay",
        "the whole frameless stream is still painted in the fallback branch",
    );
    assert_eq!(grid.cursor, (16, 0), "cursor sits past the painted text");
}

#[test]
fn wide_glyphs_survive_reflow() {
    // Three fullwidth CJK glyphs plus a trailing ASCII char, first painted at a
    // NARROW width that forces the 7-column run (`好好好` = 6 cols + `x`) to wrap
    // across rows, then a resize WIDE enough to unwrap it. alacritty reflows on
    // resize and must carry every wide glyph through the rewrap, in order, back
    // onto the first row — the survival-AND-placement property this leg models.
    // We assert the EXACT reconstructed run rather than just counting `好`, so a
    // wrong-order or wrong-row reflow that preserved counts would still fail.
    let text = "好好好x".as_bytes();
    let frames = vec![
        // Frame 1: width 4 cannot hold the 7-column run, so it wraps.
        FrameMark {
            byte_offset: text.len(),
            w: 4,
            h: 4,
        },
        // Frame 2: no new bytes, widened to 20 so the wrapped run reflows back.
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

    // After widening to 20 the whole run unwraps back onto the first row, in the
    // original order. The grid reconstruction surfaces each wide glyph's trailing
    // cell as a single blank spacer (alacritty stores `' '` in the WIDE_CHAR_SPACER
    // cell — see `backend_alacritty.rs`), so the expected row is `好 好 好 x`.
    // Pinning the exact string fixes both ORDER and PLACEMENT, not just survival
    // count: a reflow that dropped a glyph, reordered the run, or stranded it on a
    // later row would all fail here.
    assert_eq!(
        grid.viewport[0], "好 好 好 x",
        "the widened grid holds the reflowed run, in order, on the first row: {:?}",
        grid.viewport,
    );
}

#[test]
fn profile_advertises_reflow_and_logical_tracking() {
    let p = AlacrittyEmulator.profile();
    assert!(p.reflows);
    assert_eq!(p.cursor_tracking, CursorTracking::TracksLogicalLine);
    assert_eq!(p.ambiguous_glyph_width, 1);
}
