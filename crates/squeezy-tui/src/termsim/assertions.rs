//! §8.5 invariant assertions over a reconstructed [`Grid`].
//!
//! Each is a `fn(&Grid, …) -> Result<(), String>` so the matrix can run them
//! uniformly per (scenario × backend) at settled frames and collect the
//! failures. The four below are the core gate the alt-screen migration must
//! pass; broader §8.5 checks join this file as they land.
//!
//! The matchers operate on plain text only — the backends already stripped
//! styling into `Grid` rows — so a row is "a composer horizon" or "a turn
//! divider" purely by glyph shape, exactly what a human (or the xterm.js
//! oracle) sees on screen.

use super::types::{FrameMark, Grid};

/// Glyphs the composer horizon rule dissolves through (`composer_horizon_line`
/// in `lib.rs`): a solid line easing to dashes to dots. The prompt coin (`☽`
/// on an empty composer) rides the left end immediately before them.
const HORIZON_DASHES: [char; 3] = ['─', '╌', '┈'];

/// The crescent that opens both the composer horizon and rides the turn
/// divider; we disambiguate the two by what *follows* it.
const MOON: char = '☽';

/// True when `row` is a live composer horizon: a `☽` followed (after optional
/// spaces) immediately by one of the horizon dash glyphs. The `/☽\s*[─╌┈]/`
/// pattern from §8.5, encoded without a regex dependency.
///
/// This deliberately does *not* match the turn divider (`╰─☽ Worked for …`):
/// there the `☽` is followed by a space and then the word "Worked", never a
/// dash, so a settled committed turn in the viewport is not miscounted as a
/// second horizon.
fn is_composer_horizon(row: &str) -> bool {
    let mut chars = row.chars().peekable();
    while let Some(c) = chars.next() {
        if c != MOON {
            continue;
        }
        // Skip any run of spaces after the moon, then require a dash glyph.
        let mut lookahead = chars.clone();
        while matches!(lookahead.peek(), Some(' ')) {
            lookahead.next();
        }
        if matches!(lookahead.peek(), Some(d) if HORIZON_DASHES.contains(d)) {
            return true;
        }
    }
    false
}

/// At most one live composer horizon: count viewport rows that are a composer
/// horizon and fail if more than one. This encodes the stacked-divider
/// regression (VS Code / xterm.js drifting the cursor and re-emitting the
/// composer) directly.
pub(crate) fn at_most_one_composer_horizon(grid: &Grid) -> Result<(), String> {
    let matches: Vec<usize> = grid
        .viewport
        .iter()
        .enumerate()
        .filter(|(_, row)| is_composer_horizon(row))
        .map(|(i, _)| i)
        .collect();
    if matches.len() > 1 {
        return Err(format!(
            "expected <= 1 composer horizon in the viewport, found {} (rows {:?})",
            matches.len(),
            matches,
        ));
    }
    Ok(())
}

/// Count viewport rows carrying a "Worked for …" / "Failed after …" /
/// "Cancelled after …" turn divider.
fn count_turn_dividers(grid: &Grid) -> usize {
    const LABELS: [&str; 3] = ["Worked for", "Failed after", "Cancelled after"];
    grid.viewport
        .iter()
        .filter(|row| LABELS.iter().any(|label| row.contains(label)))
        .count()
}

/// No duplicated turn divider beyond the scenario's legitimate count. `max`
/// is the most dividers the scenario can legitimately show in the viewport at
/// once (0 for scenarios that commit no turn, 1 for a single committed turn).
///
/// In the inline append-only model committed turns flush to scrollback, so a
/// settled viewport should usually show 0 dividers; we assert against an upper
/// bound rather than an exact count so a divider that legitimately lingers in
/// the live region on the frame it commits is not a false failure.
pub(crate) fn no_duplicate_turn_divider(grid: &Grid, max: usize) -> Result<(), String> {
    let found = count_turn_dividers(grid);
    if found > max {
        return Err(format!(
            "expected <= {max} turn divider(s) in the viewport, found {found}",
        ));
    }
    Ok(())
}

/// The latest assistant response's known tail substring must appear somewhere
/// in `viewport ∪ scrollback` after any resize. An empty `expected_tail`
/// (scenario committed no assistant text) passes vacuously.
///
/// The needle is matched against each row independently and also against the
/// rows joined by newlines, so a tail that survives a reflow either on one row
/// or split across a wrap boundary is still found.
pub(crate) fn latest_response_present(grid: &Grid, expected_tail: &str) -> Result<(), String> {
    if expected_tail.is_empty() {
        return Ok(());
    }
    let in_rows = grid
        .scrollback
        .iter()
        .chain(grid.viewport.iter())
        .any(|row| row.contains(expected_tail));
    if in_rows {
        return Ok(());
    }
    // Reflow can split a logical line across rows; join and retry so a needle
    // straddling a wrap boundary is still recognized as present.
    let joined: String = grid
        .scrollback
        .iter()
        .chain(grid.viewport.iter())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if joined.contains(expected_tail) {
        return Ok(());
    }
    Err(format!(
        "latest assistant response tail {expected_tail:?} not found in viewport+scrollback",
    ))
}

/// The cursor row must stay within `[0, h)`, never orphaned below content.
/// `mark.h` is the terminal height in effect for the frame the grid was read
/// from.
pub(crate) fn cursor_row_in_bounds(grid: &Grid, mark: FrameMark) -> Result<(), String> {
    let (_, row) = grid.cursor;
    if mark.h == 0 {
        return Ok(());
    }
    if row >= mark.h {
        return Err(format!(
            "cursor row {row} escaped the viewport bounds [0, {})",
            mark.h,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::termsim::types::Grid;

    fn grid_with_viewport(rows: &[&str]) -> Grid {
        Grid {
            viewport: rows.iter().map(|s| s.to_string()).collect(),
            ..Grid::default()
        }
    }

    #[test]
    fn composer_horizon_matches_coin_then_dashes_not_turn_divider() {
        assert!(is_composer_horizon("☽────────────"));
        assert!(is_composer_horizon("☽ ╌╌┈┈"));
        // The turn divider has the moon followed by a space then "Worked",
        // never a dash, so it is NOT a composer horizon.
        assert!(!is_composer_horizon("   ╰─☽ Worked for 2s ───────"));
        assert!(!is_composer_horizon("plain text row"));
    }

    #[test]
    fn at_most_one_horizon_passes_for_one_fails_for_two() {
        let one = grid_with_viewport(&["body line", "☽────────"]);
        assert!(at_most_one_composer_horizon(&one).is_ok());

        let two = grid_with_viewport(&["☽────────", "more body", "☽────────"]);
        assert!(at_most_one_composer_horizon(&two).is_err());
    }

    #[test]
    fn turn_divider_count_respects_max() {
        let g = grid_with_viewport(&["   ╰─☽ Worked for 2s ──", "body"]);
        assert!(no_duplicate_turn_divider(&g, 1).is_ok());
        assert!(no_duplicate_turn_divider(&g, 0).is_err());

        let two = grid_with_viewport(&["Worked for 1s", "Worked for 2s"]);
        assert!(no_duplicate_turn_divider(&two, 1).is_err());
    }

    #[test]
    fn latest_response_found_in_viewport_or_scrollback() {
        let mut g = grid_with_viewport(&["the answer tailword"]);
        assert!(latest_response_present(&g, "tailword").is_ok());
        assert!(latest_response_present(&g, "missing").is_err());
        // Empty needle passes vacuously.
        assert!(latest_response_present(&g, "").is_ok());

        g.viewport.clear();
        g.scrollback = vec!["committed tailword line".to_string()];
        assert!(latest_response_present(&g, "tailword").is_ok());
    }

    #[test]
    fn cursor_bounds_checks_against_frame_height() {
        let mark = FrameMark {
            byte_offset: 0,
            w: 80,
            h: 24,
        };
        let in_bounds = Grid {
            cursor: (5, 3),
            ..Grid::default()
        };
        assert!(cursor_row_in_bounds(&in_bounds, mark).is_ok());
        let escaped = Grid {
            cursor: (5, 24),
            ..Grid::default()
        };
        assert!(cursor_row_in_bounds(&escaped, mark).is_err());
    }
}
