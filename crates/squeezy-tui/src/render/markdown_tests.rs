use super::{render_markdown, render_markdown_full, truncate_chars};
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
        joined.contains("trace (https://example.com/so...row/terminal?with=query)"),
        "{joined}"
    );
    assert!(
        !joined.contains("wrap/badly/in/a/narrow"),
        "long url should be abbreviated in the terminal render: {joined}"
    );
}

#[test]
fn truncate_chars_keeps_char_boundary_on_multibyte() {
    assert_eq!(truncate_chars("a\u{65e5}bX", 3), "...");
    assert_eq!(
        truncate_chars("\u{1f600}\u{1f600}\u{1f600}\u{1f600}", 3),
        "..."
    );
    assert_eq!(truncate_chars("h\u{e9}llo", 4), "h...");
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

    let table_line = lines
        .iter()
        .find(|line| {
            let text = line_text(line);
            text.contains("A") && text.contains("B")
        })
        .expect("table header line");
    let table = table_line
        .spans
        .iter()
        .find(|span| !span.content.trim().is_empty())
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
        texts.iter().any(|line| line.contains("crates/squeez...")),
        "long table cells should be visibly abbreviated:\n{}",
        texts.join("\n")
    );
}

#[test]
fn markdown_full_preserves_table_cell_text() {
    let source = "\
| Dimension | repo_map | semantic graph |
|---|---|---|
| Module names | Directory names from the repository tree | Artifact IDs from Maven module dependencies |
| Symbol-level detail | Classes, fields, methods, and package-level structure | Stopped at module dependency analysis |
";
    let lines = render_markdown_full(source);
    let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

    assert!(
        joined.contains("Directory names from the repository tree"),
        "{joined}"
    );
    assert!(
        joined.contains("Artifact IDs from Maven module dependencies"),
        "{joined}"
    );
    assert!(
        joined.contains("Classes, fields, methods, and package-level structure"),
        "{joined}"
    );
    assert!(
        !joined.contains("..."),
        "full markdown should not abbreviate final-answer table cells: {joined}"
    );
}

#[test]
fn markdown_full_preserves_inline_code_spacing() {
    let lines = render_markdown_full("Keep `│    │            │` aligned.");
    let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

    assert!(
        joined.contains("│    │            │"),
        "full markdown must preserve model-authored inline code spacing: {joined}"
    );
    assert!(
        !joined.contains("│ │ │"),
        "full markdown must not collapse inline code connector spacing: {joined}"
    );
}

#[test]
fn markdown_keeps_four_column_tables_inside_eval_capture_width() {
    let source = "\
| Surface | State | Problem | Confidence |
|---|---|---|---|
| status bar | hard | floods table like citations and commands | label_missing |
";
    let lines = render_markdown(source);
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    let longest = texts.iter().map(String::len).max().unwrap_or(0);
    assert!(
        longest <= 52,
        "four-column eval tables should fit the fixture width:\n{}",
        texts.join("\n")
    );
    assert!(
        texts.iter().any(|line| line.contains(" | ")),
        "table rows should remain row-shaped:\n{}",
        texts.join("\n")
    );
}

#[test]
fn markdown_preserves_confidence_and_code_styles_inside_tables() {
    let source = "\
| Item | Value |
|---|---|
| confidence | label_missing |
| code | `session-01234567-89ab-cdef-0123-456789abcdef` |
";
    let lines = render_markdown(source);
    let label_span = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "label_missing")
        .expect("confidence label in table");
    assert_eq!(label_span.style.fg, Some(crate::render::theme::red()));

    let code_span = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.contains("session-"))
        .expect("inline code in table");
    assert_eq!(code_span.style.fg, Some(crate::render::theme::quiet()));
}

#[test]
fn markdown_compacts_long_unbroken_tokens_in_prose() {
    let token = "LongIdentifierWithoutBreaksForViewportRegressionTestingAlphaBetaGammaDeltaEpsilon";
    let lines = render_markdown(&format!("session {token} done"));
    let joined: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
    assert!(joined.contains("..."), "{joined}");
    assert!(
        !joined.contains(token),
        "long unbroken tokens should not dominate the viewport: {joined}"
    );
}

#[test]
fn markdown_styles_unordered_bullet_markers() {
    let lines = render_markdown("- first\n- second");
    let marker = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "- ")
        .expect("bullet marker");
    assert_eq!(marker.style.fg, Some(crate::render::theme::blue()));
}

#[test]
fn markdown_colors_standalone_confidence_labels_in_prose() {
    let lines = render_markdown("The graph label is label_missing in this response.");
    let span = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "label_missing")
        .expect("standalone confidence label span");
    assert_eq!(span.style.fg, Some(crate::render::theme::red()));
}

#[test]
fn markdown_skips_embedded_confidence_label_before_valid_match() {
    let lines = render_markdown("notexact_syntax is plain, but [exact_syntax] is a label.");
    let spans: Vec<_> = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter(|span| span.content.as_ref() == "exact_syntax")
        .collect();
    assert_eq!(
        spans.len(),
        1,
        "only the bracketed label should be split out"
    );
    assert_eq!(spans[0].style.fg, Some(crate::render::theme::green()));
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
