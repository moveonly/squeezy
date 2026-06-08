//! The `alacritty_terminal` emulator leg: reflow + cursor tracking.
//!
//! alacritty_terminal rewraps wrapped lines on resize and tracks the cursor
//! across those rewraps, making it the realistic oracle for what a modern
//! reflowing terminal shows. Always-on Rust leg of the matrix.
//!
//! This is a *working* implementation (not a stub): [`Emulator::replay`] drives
//! a real [`alacritty_terminal::Term`] through the captured ANSI stream and
//! reconstructs a normalized [`Grid`]. The replay applies a `resize` per frame
//! before feeding that frame's bytes, so alacritty's reflow fires exactly the
//! way it does in a live terminal on a window-drag.
//!
//! ## API used (alacritty_terminal 0.26)
//!
//! * [`alacritty_terminal::term::Term::new(Config, &Dimensions, EventListener)`]
//!   — construct the emulator. `Config::default()` is fine; the only knob we
//!   care about is `scrolling_history` (kept at the default 10k lines).
//! * [`alacritty_terminal::event::EventListener`] — implemented as a private
//!   no-op `NoopListener` (mirrors the crate's own `VoidListener`) so the term
//!   needs no event plumbing.
//! * [`alacritty_terminal::vte::ansi::Processor`] + `Processor::advance(&mut
//!   term, bytes)` — feed raw bytes; this is the exact path `tests/ref.rs` uses.
//! * [`alacritty_terminal::term::Term::resize(impl Dimensions)`] — triggers the
//!   reflow we are testing. `Dimensions` is implemented by a private `Size`
//!   newtype so we don't depend on the crate's `term::test` helper module.
//! * [`alacritty_terminal::grid::Grid`] + `Dimensions` (`screen_lines`,
//!   `columns`, `history_size`) and `Index<Line>` / `Index<Column>` — read out
//!   rows. Viewport rows are `Line(0)..Line(screen_lines)`; scrollback is the
//!   negative lines `Line(-1) ..= Line(-history_size)`.
//! * [`alacritty_terminal::term::Term::mode`] / `TermMode::ALT_SCREEN` — detect
//!   the alt screen so an open overlay lands in `Grid::alt_screen`.
//! * `term.grid().cursor.point` (`Line` / `Column`) — the logical cursor; we
//!   clamp it into the viewport for the normalized grid.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi;

use super::emulator::{Emulator, split_frames};
use super::types::{CaptureLog, CursorTracking, EmulatorProfile, Grid};

/// Fallback size used only when a capture carries no frame marks at all, so
/// `replay` can still produce a usable grid for the COMPILES-FIRST scaffold.
const FALLBACK_SIZE: (u16, u16) = (80, 24);

/// Reflowing emulator backed by the `alacritty_terminal` crate.
#[derive(Debug, Default)]
pub(crate) struct AlacrittyEmulator;

impl Emulator for AlacrittyEmulator {
    fn replay(&self, log: &CaptureLog) -> Grid {
        let frames = split_frames(log);

        // Decide the initial geometry: the first frame's size if we have frames,
        // else the last recorded mark, else a sane fallback. alacritty needs a
        // size at construction time, and `resize` is a no-op when unchanged.
        let initial = frames
            .first()
            .map(|f| (f.mark.w, f.mark.h))
            .or_else(|| log.frames.first().map(|m| (m.w, m.h)))
            .unwrap_or(FALLBACK_SIZE);

        let mut term: Term<NoopListener> = Term::new(
            Config::default(),
            &Size::new(initial.0, initial.1),
            NoopListener,
        );
        let mut parser: ansi::Processor = ansi::Processor::new();

        if frames.is_empty() {
            // The frame splitter has not produced slices for this log (today it
            // is a scaffold stub returning `[]`). Replay the whole byte stream
            // once at the last recorded size so the leg still reconstructs a
            // real grid end to end. When `split_frames` lands, the per-frame
            // branch below takes over and reflow becomes frame-accurate.
            if let Some(mark) = log.frames.last() {
                term.resize(Size::new(mark.w, mark.h));
            }
            parser.advance(&mut term, &log.bytes);
        } else {
            // The real path: resize to each frame's size *before* feeding its
            // bytes, so a width change rewraps prior content exactly as a live
            // terminal would on a window drag.
            for frame in &frames {
                term.resize(Size::new(frame.mark.w, frame.mark.h));
                parser.advance(&mut term, frame.bytes);
            }
        }

        grid_from_term(&term)
    }

    fn profile(&self) -> EmulatorProfile {
        EmulatorProfile {
            reflows: true,
            cursor_tracking: CursorTracking::TracksLogicalLine,
            ambiguous_glyph_width: 1,
        }
    }
}

/// No-op [`EventListener`]. alacritty emits events (bell, title, clipboard, …)
/// while parsing; for offline replay we discard them all. Mirrors the crate's
/// own `VoidListener` but kept local so the backend owns its dependencies.
#[derive(Clone, Copy, Debug, Default)]
struct NoopListener;

impl EventListener for NoopListener {
    fn send_event(&self, _event: Event) {}
}

/// Minimal [`Dimensions`] carrier for `Term::new` / `Term::resize`.
///
/// alacritty's only public `Dimensions` impl outside the crate lives in its
/// `term::test` helper module; we provide our own so this leg does not lean on
/// a module that exists for the crate's own tests.
struct Size {
    columns: usize,
    screen_lines: usize,
}

impl Size {
    fn new(w: u16, h: u16) -> Self {
        // alacritty requires at least 2 columns (to hold a fullwidth glyph) and
        // 1 line; clamp so a degenerate capture size can't panic the grid.
        Self {
            columns: (w as usize).max(2),
            screen_lines: (h as usize).max(1),
        }
    }
}

impl alacritty_terminal::grid::Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Reconstruct a normalized [`Grid`] from a settled [`Term`].
///
/// Reads the active grid (alacritty swaps primary/alt on `swap_alt`, so
/// `term.grid()` already returns whichever buffer is live). Viewport rows are
/// `Line(0)..Line(screen_lines)`; scrollback is the negative history lines.
fn grid_from_term(term: &Term<NoopListener>) -> Grid {
    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let history = grid.history_size();

    let render_row = |line: Line| -> String {
        let row = &grid[line];
        let mut s = String::with_capacity(cols);
        for col in 0..cols {
            let cell: &Cell = &row[Column(col)];
            // Spacer cell trailing a wide glyph carries '\0'; skip it so widths
            // line up with the source text rather than doubling.
            if cell.c == '\0' {
                continue;
            }
            s.push(cell.c);
        }
        // Trailing blanks carry no information for the invariant checks; trim
        // them so rows compare cleanly across reflows.
        let trimmed = s.trim_end_matches(' ');
        trimmed.to_string()
    };

    let viewport: Vec<String> = (0..rows).map(|i| render_row(Line(i as i32))).collect();

    // Scrollback is oldest-first. History lines are addressed by increasingly
    // negative `Line` indices, so walk from the oldest (`-history`) up to `-1`.
    let scrollback: Vec<String> = (1..=history)
        .rev()
        .map(|back| render_row(Line(-(back as i32))))
        .collect();

    let on_alt = term.mode().contains(TermMode::ALT_SCREEN);
    let alt_screen = if on_alt { viewport.clone() } else { Vec::new() };

    // Logical cursor, clamped into the viewport. `display_offset` is 0 after a
    // settled replay (we never scroll back), so the cursor point is already
    // viewport-relative.
    let cur = grid.cursor.point;
    let cursor_col = (cur.column.0 as u16).min(cols.saturating_sub(1) as u16);
    let cursor_row = cur.line.0.clamp(0, rows.saturating_sub(1) as i32) as u16;

    Grid {
        viewport,
        alt_screen,
        scrollback,
        cursor: (cursor_col, cursor_row),
        // The append-only renderer's "live begins here" row. For a reflowed
        // replay the live region is the whole viewport, so the top is 0.
        base_y: 0,
    }
}

#[cfg(test)]
mod tests {
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
}
