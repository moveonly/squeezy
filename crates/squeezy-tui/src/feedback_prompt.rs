use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::render::palette;

pub(crate) fn menu_lines(feedback_id: &str, message_preview: String) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled(
                "Send feedback?",
                Style::default()
                    .fg(crate::render::theme::secondary())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {feedback_id}"),
                Style::default().fg(crate::render::theme::quiet()),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(message_preview, Style::default().fg(palette::muted_fg())),
        ]),
        // Keybind hint — styled as a footer row (2-space indent, footer tier,
        // no `›` cursor) so it reads as guidance, not a selectable option. The
        // prior `› ` marker made this line look like a highlighted choice.
        Line::from(Span::styled(
            "  Enter/Y send · Esc/N discard",
            Style::default().fg(crate::render::theme::footer()),
        )),
    ]
}
