use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::render::{cache, highlight, palette};

/// Render Markdown source as styled ratatui `Line`s.
///
/// Invariant: spans are built directly from `pulldown_cmark` events into
/// `Span<'static>` / `Line<'static>` — there is no intermediate AST or ANSI
/// string round-trip. `push_text` (below) emits `Span::styled` per inline
/// run, and `finish_line` commits a `Line` per `\n`. Fenced blocks delegate
/// to `highlight::highlight_code`, which preserves the same invariant.
///
/// Contributors: keep this path Span-native. Do not introduce a `String`-
/// shaped intermediate "for symmetry" with non-TUI consumers; ratatui already
/// carries `Style` structurally and any ANSI encode/decode hop is pure cost.
pub fn render_markdown(source: &str) -> Vec<Line<'static>> {
    cache::get_or_compute_markdown(source, || render_markdown_uncached(source))
}

fn render_markdown_uncached(source: &str) -> Vec<Line<'static>> {
    let mut writer = Writer::default();
    let options = Options::ENABLE_TABLES;
    for event in Parser::new_ext(source, options) {
        writer.event(event);
    }
    writer.finish()
}

#[derive(Default)]
struct Writer {
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    current_style: Style,
    list_stack: Vec<ListState>,
    quote_depth: usize,
    code_block: Option<CodeBlock>,
    /// Stack of links currently open. Each entry holds the destination URL we
    /// must append once the inner text events have been emitted.
    link_stack: Vec<String>,
    /// Active table builder. While set, all inline text routes into the
    /// builder instead of the global span buffer.
    table: Option<TableBuilder>,
}

#[derive(Clone, Debug)]
struct ListState {
    ordered: bool,
    next: u64,
}

#[derive(Clone, Debug)]
struct CodeBlock {
    language: Option<String>,
    source: String,
}

/// Accumulator for GFM tables. Cells are collected as plain strings (style is
/// dropped for the table render) so that columns can be width-padded and
/// joined with ` | ` separators. A `---` divider is emitted between header
/// and body rows.
#[derive(Default, Debug)]
struct TableBuilder {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    /// Row currently being built (one entry per `TableRow`/`TableHead`).
    current_row: Vec<String>,
    /// Cell currently being built (one entry per `TableCell`).
    current_cell: String,
    /// True while inside `TableHead` so cells flush into `headers` rather
    /// than `rows`.
    in_header: bool,
}

impl TableBuilder {
    fn push_text(&mut self, text: &str) {
        // Markdown soft/hard breaks inside a table cell should not introduce
        // newlines in the rendered grid; collapse them to single spaces.
        for ch in text.chars() {
            if ch == '\n' || ch == '\r' {
                if !self.current_cell.ends_with(' ') {
                    self.current_cell.push(' ');
                }
            } else {
                self.current_cell.push(ch);
            }
        }
    }

    fn finish_cell(&mut self) {
        let cell = std::mem::take(&mut self.current_cell);
        self.current_row.push(cell);
    }

    fn finish_row(&mut self) {
        let row = std::mem::take(&mut self.current_row);
        if self.in_header {
            self.headers = row;
        } else {
            self.rows.push(row);
        }
    }

    fn render(self, style: Style) -> Vec<Line<'static>> {
        let col_count = self
            .headers
            .len()
            .max(self.rows.iter().map(Vec::len).max().unwrap_or(0));
        if col_count == 0 {
            return Vec::new();
        }
        let mut widths = vec![0usize; col_count];
        // Compute column widths via display char counts (treat each char as 1).
        for (i, width) in widths.iter_mut().enumerate() {
            let header_cell = self.headers.get(i).map(String::as_str).unwrap_or("");
            *width = (*width).max(header_cell.chars().count());
            for row in &self.rows {
                let body_cell = row.get(i).map(String::as_str).unwrap_or("");
                *width = (*width).max(body_cell.chars().count());
            }
        }

        let render_row = |row: &[String]| -> Line<'static> {
            let mut buf = String::new();
            for (i, width) in widths.iter().enumerate() {
                if i > 0 {
                    buf.push_str(" | ");
                }
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                buf.push_str(cell);
                let cell_width = cell.chars().count();
                if cell_width < *width {
                    buf.extend(std::iter::repeat_n(' ', *width - cell_width));
                }
            }
            Line::from(Span::styled(buf, style))
        };

        let mut out = Vec::new();
        if !self.headers.is_empty() {
            out.push(render_row(&self.headers));
            let mut divider = String::new();
            for (i, width) in widths.iter().enumerate() {
                if i > 0 {
                    divider.push_str("-+-");
                }
                divider.extend(std::iter::repeat_n('-', (*width).max(3)));
            }
            out.push(Line::from(Span::styled(
                divider,
                style.patch(Style::default().fg(palette::QUIET)),
            )));
        }
        for row in &self.rows {
            out.push(render_row(row));
        }
        out
    }
}

impl Writer {
    fn event(&mut self, event: Event<'_>) {
        if self.code_block.is_some() {
            self.code_event(event);
            return;
        }

        // While inside a table, route all inline text (Text/Code/Html/Math/
        // breaks/etc.) into the active cell. Start/End tag events still flow
        // through `start`/`end` so we observe `TableCell`/`TableRow`/`TableHead`
        // boundaries.
        if self.table.is_some() {
            match event {
                Event::Start(tag) => self.start(tag),
                Event::End(tag) => self.end(tag),
                Event::Text(text)
                | Event::Code(text)
                | Event::Html(text)
                | Event::InlineHtml(text)
                | Event::InlineMath(text)
                | Event::DisplayMath(text)
                | Event::FootnoteReference(text) => {
                    if let Some(table) = self.table.as_mut() {
                        table.push_text(&text);
                    }
                }
                Event::SoftBreak | Event::HardBreak => {
                    if let Some(table) = self.table.as_mut() {
                        table.push_text(" ");
                    }
                }
                Event::Rule | Event::TaskListMarker(_) => {}
            }
            return;
        }

        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                self.push_text_with_confidence_labels(&text, self.current_style);
            }
            Event::Code(code) => {
                self.push_text(&code, self.current_style.patch(inline_code_style()));
            }
            Event::SoftBreak | Event::HardBreak => self.finish_line(),
            Event::Rule => {
                self.finish_line();
                self.push_text("----------", Style::default().fg(palette::QUIET));
                self.finish_line();
            }
            Event::TaskListMarker(checked) => {
                // Mirror Codex: checkboxes render unstyled. The `[ ]` /
                // `[x]` glyph itself is structural enough; painting it
                // (GOLD or QUIET) competes with the item content.
                self.push_text(if checked { "[x] " } else { "[ ] " }, Style::default());
            }
            Event::InlineMath(text) | Event::DisplayMath(text) | Event::FootnoteReference(text) => {
                self.push_text(&text, self.current_style);
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => self.push_style(heading_style(level)),
            Tag::BlockQuote(_) => {
                self.quote_depth += 1;
                // Mirror Codex: block-quote *content* paints the same
                // green as the leading `> ` prefix so the whole quoted
                // run reads as one visual region.
                self.push_style(Style::default().fg(Color::Green));
            }
            Tag::CodeBlock(kind) => {
                self.finish_line();
                self.code_block = Some(CodeBlock {
                    language: code_block_language(kind),
                    source: String::new(),
                });
            }
            Tag::List(start) => self.list_stack.push(ListState {
                ordered: start.is_some(),
                next: start.unwrap_or(1),
            }),
            Tag::Item => self.start_list_item(),
            Tag::Emphasis => self.push_style(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT))
            }
            Tag::Link { dest_url, .. } => {
                self.link_stack.push(dest_url.into_string());
                // Mirror Codex: link text reads in the same `Cyan` as
                // inline code, with `underlined` to disambiguate from
                // identifiers. The trailing `(url)` follows the same
                // style because it's pushed via `current_style`.
                self.push_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Tag::Image { .. } => {}
            Tag::Table(_) => {
                self.finish_line();
                self.table = Some(TableBuilder::default());
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_header = true;
                }
            }
            Tag::TableRow => {}
            Tag::TableCell => {}
            Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::HtmlBlock
            | Tag::MetadataBlock(_) => {}
            Tag::Superscript => self.push_style(Style::default()),
            Tag::Subscript => self.push_style(Style::default()),
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item => self.finish_line(),
            TagEnd::BlockQuote(_) => {
                self.finish_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.pop_style();
            }
            TagEnd::CodeBlock => self.finish_code_block(),
            TagEnd::List(_) => {
                self.finish_line();
                self.list_stack.pop();
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript => self.pop_style(),
            TagEnd::Link => {
                if let Some(url) = self.link_stack.pop()
                    && !url.is_empty()
                {
                    let text = format!(" ({url})");
                    if let Some(table) = self.table.as_mut() {
                        table.push_text(&text);
                    } else {
                        self.push_text(&text, self.current_style);
                    }
                }
                self.pop_style();
            }
            TagEnd::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.finish_cell();
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.finish_row();
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.finish_row();
                    table.in_header = false;
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.finish_line();
                    let rendered = table.render(self.current_style);
                    self.lines.extend(rendered);
                }
            }
            TagEnd::Image
            | TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn code_event(&mut self, event: Event<'_>) {
        match event {
            Event::End(TagEnd::CodeBlock) => self.finish_code_block(),
            Event::Text(text) | Event::Code(text) => {
                if let Some(block) = self.code_block.as_mut() {
                    block.source.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(block) = self.code_block.as_mut() {
                    block.source.push('\n');
                }
            }
            _ => {}
        }
    }

    fn start_list_item(&mut self) {
        self.finish_line();
        let depth = self.list_stack.len().saturating_sub(1);
        if depth > 0 {
            self.push_text(&"  ".repeat(depth), Style::default());
        }
        let (marker, ordered) = if let Some(list) = self.list_stack.last_mut() {
            if list.ordered {
                let marker = format!("{}. ", list.next);
                list.next += 1;
                (marker, true)
            } else {
                ("- ".to_string(), false)
            }
        } else {
            ("- ".to_string(), false)
        };
        // Mirror Codex's discipline: ordered markers carry a single
        // semantic color (`LightBlue` for "this is a numbered step");
        // unordered markers stay unstyled so a long bullet list reads as
        // structure, not as a wall of color. The prior `GOLD` painted
        // every marker bright yellow on every line.
        let style = if ordered {
            Style::default().fg(Color::LightBlue)
        } else {
            Style::default()
        };
        self.push_text(&marker, style);
    }

    fn push_style(&mut self, style: Style) {
        self.style_stack.push(self.current_style);
        self.current_style = self.current_style.patch(style);
    }

    fn pop_style(&mut self) {
        self.current_style = self.style_stack.pop().unwrap_or_default();
    }

    /// Render `text` while colouring graph confidence labels (`exact_syntax`,
    /// `candidate_set`, `label_missing`, …) so a watcher can scan a turn for
    /// quality at a glance instead of reading every clause.
    ///
    /// Two forms are recognised:
    ///   1. `… — exact_syntax …` — the em-dash separator survives parsing,
    ///      so we find it inside a single Event::Text.
    ///   2. `[exact_syntax]` — pulldown_cmark splits this into three
    ///      separate `[`, `label`, `]` text events, so we colour the
    ///      whole event when its content is exactly a known label.
    fn push_text_with_confidence_labels(&mut self, text: &str, base_style: Style) {
        if let Some(label) = confidence_label_exact_match(text) {
            let label_style = base_style.patch(Style::default().fg(confidence_label_color(label)));
            self.push_text(text, label_style);
            return;
        }
        let mut cursor = 0;
        while let Some((start, end, label)) = find_next_confidence_label(text, cursor) {
            if start > cursor {
                self.push_text(&text[cursor..start], base_style);
            }
            let color = confidence_label_color(label);
            let label_style = base_style.patch(Style::default().fg(color));
            self.push_text(&text[start..end], label_style);
            cursor = end;
        }
        if cursor < text.len() {
            self.push_text(&text[cursor..], base_style);
        }
    }

    fn push_text(&mut self, text: &str, style: Style) {
        for segment in text.split_inclusive('\n') {
            self.ensure_quote_prefix();
            if let Some(line) = segment.strip_suffix('\n') {
                if !line.is_empty() {
                    self.spans.push(Span::styled(line.to_string(), style));
                }
                self.finish_line();
            } else if !segment.is_empty() {
                self.spans.push(Span::styled(segment.to_string(), style));
            }
        }
    }

    fn ensure_quote_prefix(&mut self) {
        if self.spans.is_empty() && self.quote_depth > 0 {
            self.spans.push(Span::styled(
                "> ".repeat(self.quote_depth),
                Style::default().fg(Color::Green),
            ));
        }
    }

    fn finish_line(&mut self) {
        if !self.spans.is_empty() {
            self.lines.push(Line::from(std::mem::take(&mut self.spans)));
        }
    }

    fn finish_code_block(&mut self) {
        let Some(block) = self.code_block.take() else {
            return;
        };
        let source = block.source.trim_end_matches('\n');
        let code_lines = highlight::highlight_code(block.language.as_deref(), source);
        self.lines.extend(code_lines);
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.finish_line();
        if self.lines.is_empty() {
            self.lines.push(Line::from(""));
        }
        self.lines
    }
}

fn heading_style(level: HeadingLevel) -> Style {
    // Mirror Codex: hierarchy through modifiers, not color. A wall of
    // GOLD-painted headings competed with everything else for the eye.
    // Bold for every level; underline H1/H2; italic for H3-H6 so the
    // deeper levels still differ from H2.
    let mut style = Style::default().add_modifier(Modifier::BOLD);
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 => {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        _ => {
            style = style.add_modifier(Modifier::ITALIC);
        }
    }
    style
}

fn inline_code_style() -> Style {
    // Mirror Codex: inline code is the one place a single semantic color
    // earns its keep — the eye learns "cyan = identifier" instantly.
    // Standard `Cyan`, not bright/light, so it sits beside prose
    // without screaming.
    Style::default().fg(Color::Cyan)
}

/// Graph confidence labels squeezy emits in assistant prose
/// (`exact_syntax`, `import_resolved`, `candidate_set`, `external`,
/// `unknown`, `label_missing`). The renderer highlights any of these
/// when they appear:
///   * preceded by an em dash and space (`X — exact_syntax`), or
///   * wrapped in square brackets (`[exact_syntax]`).
///
/// The label itself is the part that gets the palette colour; the
/// surrounding punctuation keeps the inherited style.
const CONFIDENCE_LABELS: &[&str] = &[
    "exact_syntax",
    "import_resolved",
    "candidate_set",
    "external",
    "unknown",
    "label_missing",
];

/// Returns the label if `text` is exactly one of the known confidence
/// labels (used by the bracketed form, which pulldown_cmark splits
/// into separate `[`, `label`, `]` text events).
fn confidence_label_exact_match(text: &str) -> Option<&'static str> {
    CONFIDENCE_LABELS
        .iter()
        .copied()
        .find(|label| *label == text)
}

fn confidence_label_color(label: &str) -> Color {
    match label {
        "exact_syntax" => palette::SUCCESS_GREEN,
        "import_resolved" => palette::AMBER,
        "candidate_set" => palette::GOLD,
        "external" | "unknown" => palette::QUIET,
        "label_missing" => palette::ERROR_RED,
        _ => palette::QUIET,
    }
}

/// Locate the next confidence label in `text` starting from `from`.
/// Returns the byte range to style and the matched label string.
fn find_next_confidence_label(text: &str, from: usize) -> Option<(usize, usize, &'static str)> {
    let haystack = &text[from..];
    let mut best: Option<(usize, usize, &'static str)> = None;
    for &label in CONFIDENCE_LABELS {
        // `— label` form: match the literal " — label" or " — label" with
        // an em dash; require a leading separator so we don't colour
        // bare matches inside identifiers (`my_exact_syntax_test`).
        let with_em_dash = format!(" — {label}");
        if let Some(off) = haystack.find(&with_em_dash) {
            // Highlight just the label, not the em-dash separator.
            let label_start = from + off + with_em_dash.len() - label.len();
            let label_end = label_start + label.len();
            if !is_identifier_continuation(text, label_end) {
                pick_earliest(&mut best, (label_start, label_end, label));
            }
        }
        let bracketed = format!("[{label}]");
        if let Some(off) = haystack.find(&bracketed) {
            // Skip the leading `[`; colour `label` only (the brackets
            // stay in the inherited style so the punctuation reads
            // normally).
            let label_start = from + off + 1;
            let label_end = label_start + label.len();
            pick_earliest(&mut best, (label_start, label_end, label));
        }
    }
    best
}

fn pick_earliest<'a>(
    best: &mut Option<(usize, usize, &'a str)>,
    candidate: (usize, usize, &'a str),
) {
    match best {
        Some(current) if current.0 <= candidate.0 => {}
        _ => *best = Some(candidate),
    }
}

/// Returns true when the byte after `end` is alphanumeric or `_` — i.e.
/// we're still inside an identifier, so the apparent label is actually
/// the prefix of a longer word (`exact_syntax_foo`).
fn is_identifier_continuation(text: &str, end: usize) -> bool {
    text.as_bytes()
        .get(end)
        .map(|b| b.is_ascii_alphanumeric() || *b == b'_')
        .unwrap_or(false)
}

fn code_block_language(kind: CodeBlockKind<'_>) -> Option<String> {
    match kind {
        CodeBlockKind::Fenced(info) => info
            .split_whitespace()
            .next()
            .map(str::trim)
            .filter(|language| !language.is_empty())
            .map(ToOwned::to_owned),
        CodeBlockKind::Indented => None,
    }
}

#[cfg(test)]
#[path = "markdown_tests.rs"]
mod tests;
