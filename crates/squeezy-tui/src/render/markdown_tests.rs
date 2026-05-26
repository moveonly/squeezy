use super::render_markdown;
use ratatui::text::Line;

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

#[test]
fn markdown_renders_inline_link_text_and_url() {
    let lines = render_markdown("Click [here](https://example.com) please.");
    let joined: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
    assert!(
        joined.contains("here"),
        "link text should be preserved: {joined}"
    );
    assert!(
        joined.contains("https://example.com"),
        "link url should be appended: {joined}"
    );
    // Acceptance form: `text (url)`.
    assert!(
        joined.contains("here (https://example.com)"),
        "link should render as `text (url)`: {joined}"
    );
}

#[test]
fn markdown_renders_three_column_table_with_separators_and_divider() {
    let source = "\
| Feature | Codex | Squeezy |
|---|---|---|
| Tables | yes | yes |
| Links | yes | yes |
";
    let lines = render_markdown(source);
    let texts: Vec<String> = lines.iter().map(line_text).collect();

    // Find the header row.
    let header_idx = texts
        .iter()
        .position(|t| t.contains("Feature") && t.contains("Codex") && t.contains("Squeezy"))
        .unwrap_or_else(|| panic!("header row missing in:\n{}", texts.join("\n")));
    let header_line = &texts[header_idx];
    assert!(
        header_line.matches(" | ").count() >= 2,
        "header row should use ` | ` separators between three columns: {header_line:?}"
    );

    // Divider line immediately after the header.
    let divider_line = texts
        .get(header_idx + 1)
        .unwrap_or_else(|| panic!("missing divider after header in:\n{}", texts.join("\n")));
    assert!(
        divider_line.contains("---"),
        "header divider should contain dashes: {divider_line:?}"
    );

    // Each body row must contain its cells and at least two ` | ` separators.
    let row1 = texts
        .iter()
        .find(|t| t.contains("Tables"))
        .unwrap_or_else(|| panic!("first body row missing in:\n{}", texts.join("\n")));
    assert!(
        row1.matches(" | ").count() >= 2,
        "body row 1 should have ` | ` between three cells: {row1:?}"
    );
    let row2 = texts
        .iter()
        .find(|t| t.contains("Links"))
        .unwrap_or_else(|| panic!("second body row missing in:\n{}", texts.join("\n")));
    assert!(
        row2.matches(" | ").count() >= 2,
        "body row 2 should have ` | ` between three cells: {row2:?}"
    );
}
