use super::*;

#[test]
fn render_styled_produces_lines_and_ansi() {
    let (lines, ansi) = render_styled("Hello **world** and `code`.\n");
    assert!(!lines.is_empty());
    let all_spans: Vec<&StyledSpan> = lines.iter().flat_map(|l| l.spans.iter()).collect();
    assert!(all_spans.iter().any(|s| s.text.contains("Hello")));
    // Bold or italic modifiers should show up somewhere in the styled
    // span list for a markdown sample containing `**world**`.
    let has_modifier = all_spans.iter().any(|s| !s.modifiers.is_empty());
    assert!(has_modifier, "expected at least one styled span");
    // ANSI string should carry an SGR escape and a reset.
    assert!(ansi.contains("\x1b["), "expected SGR escape in ANSI output");
    assert!(ansi.contains("\x1b[0m"), "expected reset in ANSI output");
}

#[test]
fn render_styled_plain_text() {
    let (lines, ansi) = render_styled("just plain text\n");
    assert!(!lines.is_empty());
    // Plain text has no styling but should still produce non-empty spans.
    let all_spans: Vec<&StyledSpan> = lines.iter().flat_map(|l| l.spans.iter()).collect();
    assert!(all_spans.iter().any(|s| s.text.contains("plain text")));
    assert!(ansi.contains("plain text"));
}

#[test]
fn push_ansi_preserves_combined_sgr_order() {
    let style = Style::default()
        .fg(Color::Rgb(1, 2, 3))
        .bg(Color::Indexed(4))
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let mut ansi = String::new();

    push_ansi(&mut ansi, &style, "x");

    assert_eq!(ansi, "\x1b[38;2;1;2;3;48;5;4;1;4mx\x1b[0m");
}
