use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::*;

#[test]
fn card_background_preserves_existing_line_background() {
    let diff_bg = Style::default().bg(crate::render::theme::green());
    let card_bg = Style::default().bg(Color::Blue);
    let line = Line::from(vec![
        Span::styled("│ ", Style::default().fg(Color::Gray)),
        Span::styled("+", diff_bg),
        Span::styled("new", diff_bg),
    ])
    .style(diff_bg);

    let styled = apply_background(line, Some(card_bg));

    assert_eq!(styled.style.bg, Some(crate::render::theme::green()));
    for span in styled.spans {
        assert_ne!(span.style.bg, Some(Color::Blue));
    }
}

#[test]
fn card_background_preserves_existing_span_background() {
    let diff_bg = Style::default().bg(crate::render::theme::green());
    let card_bg = Style::default().bg(Color::Blue);
    let line = Line::from(vec![
        Span::styled("prefix", Style::default()),
        Span::styled("+new", diff_bg),
    ]);

    let styled = apply_background(line, Some(card_bg));

    assert_eq!(styled.style.bg, Some(Color::Blue));
    assert_eq!(styled.spans[0].style.bg, Some(Color::Blue));
    assert_eq!(
        styled.spans[1].style.bg,
        Some(crate::render::theme::green())
    );
}
