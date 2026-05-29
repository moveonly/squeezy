use super::render_markdown;
use crate::render::palette;
use ratatui::style::Modifier;
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
fn markdown_truncates_long_link_urls() {
    let lines = render_markdown(
        "See [trace](https://example.com/some/really/long/path/that/would/wrap/badly/in/a/narrow/terminal?with=query).",
    );
    let joined: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
    assert!(
        joined.contains("trace (https://example.com/some/really/long/path/tha..."),
        "{joined}"
    );
    assert!(
        !joined.contains("wrap/badly/in/a/narrow"),
        "long url should be abbreviated in the terminal render: {joined}"
    );
}

#[test]
fn markdown_heading_style_does_not_leak_into_following_blocks() {
    let lines =
        render_markdown("# Heading\n\nNormal paragraph\n\n| A | B |\n|---|---|\n| C | D |\n");
    let heading = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "Heading")
        .expect("heading span");
    assert!(heading.style.add_modifier.contains(Modifier::BOLD));

    let normal = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref().contains("Normal paragraph"))
        .expect("paragraph span");
    assert!(
        !normal.style.add_modifier.contains(Modifier::BOLD),
        "paragraph after heading should not inherit heading style"
    );

    let table = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref().contains("A") && span.content.as_ref().contains("B"))
        .expect("table header span");
    assert!(
        !table.style.add_modifier.contains(Modifier::BOLD),
        "table after heading should not inherit heading style"
    );
}

#[test]
fn markdown_caps_table_column_width_for_narrow_terminals() {
    let source = "\
| Path | Symptom | Fix |
|---|---|---|
| crates/squeezy-tui/src/render/markdown.rs | heading style leaks into every following row | pop heading style |
";
    let lines = render_markdown(source);
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    let longest = texts.iter().map(String::len).max().unwrap_or(0);
    assert!(
        longest <= 60,
        "rendered table rows should stay compact for 64-col captures:\n{}",
        texts.join("\n")
    );
    assert!(
        texts.iter().any(|line| line.contains("crates/squeezy-...")),
        "long table cells should be visibly abbreviated:\n{}",
        texts.join("\n")
    );
}

#[test]
fn markdown_colors_standalone_confidence_labels_in_prose() {
    let lines = render_markdown("The graph label is label_missing in this response.");
    let span = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "label_missing")
        .expect("standalone confidence label span");
    assert_eq!(span.style.fg, Some(palette::ERROR_RED));
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
