use std::sync::{Arc, Mutex};

use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::{CrosstermBackend, TestBackend},
    layout::Rect,
    text::{Line, Span},
};

use super::*;
use crate::terminal_writer::TerminalWriter;

#[test]
fn centered_caps_to_max_and_centers() {
    let full = Rect::new(0, 0, 200, 60);
    let area = centered(full, 160, 32);
    assert_eq!(area.width, 160);
    assert_eq!(area.height, 32);
    // Centered: equal margins on each axis.
    assert_eq!(area.x, (200 - 160) / 2);
    assert_eq!(area.y, (60 - 32) / 2);
}

#[test]
fn centered_shrinks_to_terminal_when_smaller_than_caps() {
    let full = Rect::new(0, 0, 80, 24);
    let area = centered(full, 160, 32);
    // The terminal is smaller than the caps, so the area fills it exactly.
    assert_eq!(area.width, 80);
    assert_eq!(area.height, 24);
    assert_eq!(area.x, 0);
    assert_eq!(area.y, 0);
}

#[test]
fn centered_honours_origin_offset() {
    let full = Rect::new(4, 2, 100, 40);
    let area = centered(full, 40, 10);
    assert_eq!(area.width, 40);
    assert_eq!(area.height, 10);
    assert_eq!(area.x, 4 + (100 - 40) / 2);
    assert_eq!(area.y, 2 + (40 - 10) / 2);
}

#[test]
fn surface_returns_inner_rect_inside_the_centered_block() {
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut inner = Rect::default();
    let title = Line::from(Span::raw("title"));
    terminal
        .draw(|frame| {
            let full = frame.area();
            inner = surface(frame, full, 60, 20, title.clone());
        })
        .expect("draw");

    let outer = centered(Rect::new(0, 0, 120, 40), 60, 20);
    // The block borders consume one cell on every side, so inner is inset
    // by one and two cells smaller on each axis.
    assert_eq!(inner.x, outer.x + 1);
    assert_eq!(inner.y, outer.y + 1);
    assert_eq!(inner.width, outer.width - 2);
    assert_eq!(inner.height, outer.height - 2);
}

#[test]
fn surface_draws_a_border_and_title() {
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let title = Line::from(Span::raw("hello-modal"));
    terminal
        .draw(|frame| {
            let full = frame.area();
            surface(frame, full, 60, 20, title.clone());
        })
        .expect("draw");

    let text = buffer_text(&terminal, 120, 40);
    // The rounded-border corner and the supplied title both render.
    assert!(text.contains('╭'), "expected a rounded border corner");
    assert!(text.contains("hello-modal"), "expected the title to render");
}

#[test]
fn clear_after_close_runs_against_the_crossterm_backend_terminal() {
    // `clear_after_close` is constrained to `CrosstermBackend<W>`, so it binds
    // to the same guard terminal both pickers hold. The crossterm backend
    // exposes no queryable cell buffer and its inner writer is private, so we
    // capture the emitted escape stream through the shared
    // `TerminalWriter::capture` sink (the seam production uses) and prove the
    // close-clear writes a fresh cleared frame rather than leaving ghost rows.
    let sink = Arc::new(Mutex::new(Vec::new()));
    let backend = CrosstermBackend::new(TerminalWriter::capture(Arc::clone(&sink)));
    // A fixed viewport keeps `Terminal::new`/`frame.area()` from querying the
    // real OS terminal for its size. The capture writer has no backing tty, so
    // a size probe fails with `WouldBlock` under CI's pipe-backed stdout; the
    // fixed rect makes the render deterministic and headless-safe.
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
        },
    )
    .expect("terminal");

    // Paint the modal first so there is real content to clear.
    terminal
        .draw(|frame| {
            let full = frame.area();
            surface(frame, full, 30, 6, Line::from(Span::raw("ghost-title")));
        })
        .expect("draw");
    let painted = String::from_utf8_lossy(&sink.lock().unwrap()).into_owned();
    assert!(
        painted.contains("ghost-title"),
        "precondition: the modal painted its title into the stream"
    );

    let before = sink.lock().unwrap().len();
    clear_after_close(&mut terminal).expect("clear");
    let after = sink.lock().unwrap().len();
    // The close-clear must emit a fresh frame (the clear + flush), so the
    // stream grows. A no-op would leave the byte count unchanged.
    assert!(
        after > before,
        "close-clear should emit a fresh cleared frame, before={before} after={after}"
    );
}

fn buffer_text(terminal: &Terminal<TestBackend>, width: u16, height: u16) -> String {
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..height {
        for x in 0..width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}
