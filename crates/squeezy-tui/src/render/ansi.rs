use ansi_to_tui::IntoText;
use ratatui::text::{Line, Text};

pub(crate) fn ansi_to_text(s: &str) -> Text<'static> {
    let expanded = expand_tabs(s);
    expanded
        .as_ref()
        .into_text()
        .unwrap_or_else(|_| Text::from(crate::strip_ansi_escape_sequences(expanded.as_ref())))
}

pub(crate) fn ansi_to_line(s: &str) -> Line<'static> {
    let text = ansi_to_text(s);
    text.lines
        .into_iter()
        .next()
        .unwrap_or_else(|| Line::from(""))
}

fn expand_tabs(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains('\t') {
        std::borrow::Cow::Owned(s.replace('\t', "    "))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}
