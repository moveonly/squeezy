//! Shared modal-surface helpers for the startup pickers.
//!
//! Both the resume picker (`resume_picker.rs`) and the startup model picker
//! (`startup_model_picker.rs`) render the same modal mechanic: clear the
//! whole frame, then draw a centered rounded-border block and lay their
//! content into its inner rect. Phase 6 of the alt-screen renderer plan
//! (`docs/internal/TUI_ALT_SCREEN_RENDERER_PLAN.md`) requires both to draw
//! as modal surfaces on the *same* shared fullscreen terminal and to clear
//! once after they close so no ghost rows survive into the next surface.
//!
//! This module owns that mechanic so the two render paths cannot drift:
//!
//! - [`centered`] computes the centered sub-rect (replacing the two private
//!   `centered_area` copies that differed only in their caps).
//! - [`surface`] clears the target area and draws the centered bordered
//!   block, returning the block's inner rect for the caller's content.
//! - [`clear_after_close`] performs the single deliberate clear-on-close so
//!   the picker's block is wiped exactly once when the picker returns,
//!   rather than relying on the next surface to overpaint stale rows.

use std::io;

use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Rect},
    style::Style,
    text::Line,
    widgets::{Block, BorderType, Borders, Clear},
};

use crate::render::theme;

/// Center a `max_width` x `max_height` area inside `full`, shrinking to fit
/// when the terminal is smaller than the requested caps. Centering is byte
/// identical to the previous per-picker `centered_area` helpers; callers
/// pass their own caps (resume keeps 160x32, startup keeps 98x20).
pub(crate) fn centered(full: Rect, max_width: u16, max_height: u16) -> Rect {
    let width = full.width.min(max_width);
    let height = full.height.min(max_height);
    let x = full.x + full.width.saturating_sub(width) / 2;
    let y = full.y + full.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Render the shared modal surface: clear `full`, draw a centered
/// rounded-border accent block sized by `max_width` x `max_height` with the
/// supplied `title` (left-aligned), and return the block's inner rect so the
/// caller can lay its own content into it.
pub(crate) fn surface(
    frame: &mut Frame<'_>,
    full: Rect,
    max_width: u16,
    max_height: u16,
    title: Line<'_>,
) -> Rect {
    frame.render_widget(Clear, full);

    let area = centered(full, max_width, max_height);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::accent()))
        .title(title)
        .title_alignment(Alignment::Left);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// Clear the whole terminal exactly once after a picker closes so the modal
/// block leaves no ghost rows behind. Generic over the `CrosstermBackend`
/// inner writer `W`, so it binds to the same guard terminal both pickers hold
/// in production (a `CrosstermBackend<TerminalWriter>`); the tests drive it
/// through a `CrosstermBackend<TerminalWriter::capture>` sink. A
/// `Terminal<TestBackend>` cannot satisfy this signature, so the picker
/// row/layout tests render directly instead of calling this.
pub(crate) fn clear_after_close<W: io::Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
) -> io::Result<()> {
    // `Terminal::draw` already flushes both the buffer diff and the backend
    // writer, so no separate `terminal.flush()` is needed here.
    terminal.draw(|frame| {
        let full = frame.area();
        frame.render_widget(Clear, full);
    })?;
    Ok(())
}

#[cfg(test)]
#[path = "modal_tests.rs"]
mod tests;
