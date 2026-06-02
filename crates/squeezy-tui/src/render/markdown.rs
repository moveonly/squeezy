use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::render::{cache, highlight};

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

const MAX_TABLE_COLUMN_WIDTH: usize = 18;
const MIN_TABLE_COLUMN_WIDTH: usize = 6;
const MAX_TABLE_ROW_WIDTH: usize = 52;
const MAX_LINK_URL_CHARS: usize = 48;
const MAX_UNBROKEN_TEXT_TOKEN_CHARS: usize = 56;
const MAX_INLINE_CODE_CHARS: usize = 48;

#[derive(Clone, Debug, Default)]
struct TableCell {
    runs: Vec<TableRun>,
}

#[derive(Clone, Debug)]
struct TableRun {
    text: String,
    style: Style,
}

impl TableCell {
    fn push_text(&mut self, text: &str, style: Style) {
        let mut collapsed = String::new();
        for ch in text.chars() {
            if ch == '\n' || ch == '\r' {
                if !collapsed.ends_with(' ') {
                    collapsed.push(' ');
                }
            } else {
                collapsed.push(ch);
            }
        }
        if collapsed.is_empty() {
            return;
        }
        if let Some(last) = self.runs.last_mut()
            && last.style == style
        {
            last.text.push_str(&collapsed);
            return;
        }
        self.runs.push(TableRun {
            text: collapsed,
            style,
        });
    }

    fn char_count(&self) -> usize {
        self.runs.iter().map(|run| run.text.chars().count()).sum()
    }

    fn render_spans(&self, width: usize, fallback_style: Style) -> Vec<Span<'static>> {
        let mut out = Vec::new();
        let total = self.char_count();
        let visible_limit = if total > width && width >= 3 {
            width - 3
        } else {
            width
        };
        let mut remaining = visible_limit;
        for run in &self.runs {
            if remaining == 0 {
                break;
            }
            let run_style = confidence_label_exact_match(run.text.trim())
                .map(|label| {
                    run.style
                        .patch(Style::default().fg(confidence_label_color(label)))
                })
                .unwrap_or(run.style);
            let run_chars = run.text.chars().count();
            let take = remaining.min(run_chars);
            let visible: String = run.text.chars().take(take).collect();
            for span in confidence_label_spans(&visible, run_style) {
                out.push(span);
            }
            remaining -= take;
            if take < run_chars {
                break;
            }
        }
        if total > width && width >= 3 {
            out.push(Span::styled("...", fallback_style));
        }
        let visible_width: usize = out.iter().map(|span| span.content.chars().count()).sum();
        if visible_width < width {
            out.push(Span::styled(
                " ".repeat(width - visible_width),
                fallback_style,
            ));
        }
        out
    }
}

/// Accumulator for GFM tables. Cells preserve inline styles so code spans,
/// links, and confidence labels remain visible inside the compact table render.
#[derive(Default, Debug)]
struct TableBuilder {
    headers: Vec<TableCell>,
    rows: Vec<Vec<TableCell>>,
    /// Row currently being built (one entry per `TableRow`/`TableHead`).
    current_row: Vec<TableCell>,
    /// Cell currently being built (one entry per `TableCell`).
    current_cell: TableCell,
    /// True while inside `TableHead` so cells flush into `headers` rather
    /// than `rows`.
    in_header: bool,
}

impl TableBuilder {
    fn push_text(&mut self, text: &str, style: Style) {
        self.current_cell.push_text(text, style);
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
            *width = (*width).max(self.headers.get(i).map(TableCell::char_count).unwrap_or(0));
            for row in &self.rows {
                *width = (*width).max(row.get(i).map(TableCell::char_count).unwrap_or(0));
            }
            *width = (*width).min(MAX_TABLE_COLUMN_WIDTH);
        }
        fit_table_width(&mut widths);

        let render_row = |row: &[TableCell]| -> Line<'static> {
            let mut spans = Vec::new();
            for (i, width) in widths.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(
                        " | ",
                        style.patch(Style::default().fg(crate::render::theme::quiet())),
                    ));
                }
                let rendered = row
                    .get(i)
                    .map(|cell| cell.render_spans(*width, style))
                    .unwrap_or_else(|| vec![Span::styled(" ".repeat(*width), style)]);
                spans.extend(rendered);
            }
            Line::from(spans)
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
                style.patch(Style::default().fg(crate::render::theme::quiet())),
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
                | Event::Html(text)
                | Event::InlineHtml(text)
                | Event::InlineMath(text)
                | Event::DisplayMath(text)
                | Event::FootnoteReference(text) => {
                    if let Some(table) = self.table.as_mut() {
                        table.push_text(&text, self.current_style);
                    }
                }
                Event::Code(text) => {
                    if let Some(table) = self.table.as_mut() {
                        let text = compact_inline_code(&text);
                        table.push_text(
                            &text,
                            self.current_style.patch(inline_code_style_for(&text)),
                        );
                    }
                }
                Event::SoftBreak | Event::HardBreak => {
                    if let Some(table) = self.table.as_mut() {
                        table.push_text(" ", self.current_style);
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
                let code = compact_inline_code(&code);
                self.push_text(
                    &code,
                    self.current_style.patch(inline_code_style_for(&code)),
                );
            }
            Event::SoftBreak | Event::HardBreak => self.finish_line(),
            Event::Rule => {
                self.finish_line();
                self.push_text(
                    "----------",
                    Style::default().fg(crate::render::theme::quiet()),
                );
                self.finish_line();
            }
            Event::TaskListMarker(checked) => {
                // Checkboxes render unstyled. The `[ ]` / `[x]` glyph
                // itself is structural enough; painting it (crate::render::theme::secondary() or
                // crate::render::theme::quiet()) competes with the item content.
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
                // Block-quote *content* paints the same green as the
                // leading `> ` prefix so the whole quoted run reads as
                // one visual region.
                self.push_style(Style::default().fg(crate::render::theme::green()));
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
                // Link text reads in the same accent as inline code,
                // with `underlined` to disambiguate from identifiers.
                // The trailing `(url)` follows the same style because
                // it's pushed via `current_style`.
                self.push_style(
                    Style::default()
                        .fg(crate::render::theme::inline_code())
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
            TagEnd::Paragraph | TagEnd::Item => self.finish_line(),
            TagEnd::Heading(_) => {
                self.finish_line();
                self.pop_style();
            }
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
                    let text = format!(" ({})", display_link_url(&url));
                    if let Some(table) = self.table.as_mut() {
                        table.push_text(&text, self.current_style);
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
        let (marker, _ordered) = if let Some(list) = self.list_stack.last_mut() {
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
        // Ordered and unordered markers share a quiet structural color so
        // bullets do not disappear next to numbered lists in dense output.
        let style = Style::default().fg(crate::render::theme::blue());
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
        let text = compact_unbroken_text_tokens(text);
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
                Style::default().fg(crate::render::theme::green()),
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
    // Hierarchy through modifiers, not color. A wall of crate::render::theme::secondary()-painted
    // headings competed with everything else for the eye. Bold for
    // every level; underline H1/H2; italic for H3-H6 so the deeper
    // levels still differ from H2.
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

fn inline_code_style_for(text: &str) -> Style {
    let lower = text.to_ascii_lowercase();
    let color = if looks_like_session_id(text) || lower.starts_with("session") {
        crate::render::theme::quiet()
    } else if lower.contains("model") || text.starts_with('@') {
        crate::render::theme::inline_model()
    } else if lower.contains("branch") || lower.contains("refs/") || lower.contains('/') {
        crate::render::theme::path_hint()
    } else if lower.contains("cost")
        || lower.contains("token")
        || lower.contains("ctx")
        || lower.contains("read:")
    {
        crate::render::theme::accent()
    } else {
        crate::render::theme::inline_code()
    };
    Style::default().fg(color)
}

fn compact_inline_code(text: &str) -> String {
    let collapsed = collapse_spaces(text);
    if looks_like_session_id(&collapsed) {
        middle_truncate_chars(&collapsed, 24)
    } else {
        middle_truncate_chars(&collapsed, MAX_INLINE_CODE_CHARS)
    }
}

fn looks_like_session_id(text: &str) -> bool {
    let hexish = text
        .chars()
        .filter(|c| c.is_ascii_hexdigit() || *c == '-')
        .count();
    text.len() >= 32 && text.contains('-') && hexish * 5 >= text.chars().count() * 4
}

/// Graph confidence labels squeezy emits in assistant prose
/// (`exact_syntax`, `import_resolved`, `candidate_set`, `external`,
/// `unknown`, `label_missing`). The renderer highlights any of these
/// when they appear:
///   * as a standalone prose token (`label_missing`),
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
        "exact_syntax" => crate::render::theme::green(),
        "import_resolved" => crate::render::theme::accent(),
        "candidate_set" => crate::render::theme::secondary(),
        "external" | "unknown" => crate::render::theme::quiet(),
        "label_missing" => crate::render::theme::red(),
        _ => crate::render::theme::quiet(),
    }
}

fn confidence_label_spans(text: &str, base_style: Style) -> Vec<Span<'static>> {
    if let Some(label) = confidence_label_exact_match(text) {
        return vec![Span::styled(
            text.to_string(),
            base_style.patch(Style::default().fg(confidence_label_color(label))),
        )];
    }
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some((start, end, label)) = find_next_confidence_label(text, cursor) {
        if start > cursor {
            out.push(Span::styled(text[cursor..start].to_string(), base_style));
        }
        out.push(Span::styled(
            text[start..end].to_string(),
            base_style.patch(Style::default().fg(confidence_label_color(label))),
        ));
        cursor = end;
    }
    if cursor < text.len() {
        out.push(Span::styled(text[cursor..].to_string(), base_style));
    }
    out
}

fn fit_table_width(widths: &mut [usize]) {
    if widths.is_empty() {
        return;
    }
    let separators = widths.len().saturating_sub(1) * 3;
    while widths.iter().sum::<usize>() + separators > MAX_TABLE_ROW_WIDTH {
        let Some((idx, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, width)| **width > MIN_TABLE_COLUMN_WIDTH)
            .max_by_key(|(_, width)| **width)
        else {
            break;
        };
        widths[idx] -= 1;
    }
}

/// Locate the next confidence label in `text` starting from `from`.
/// Returns the byte range to style and the matched label string.
fn find_next_confidence_label(text: &str, from: usize) -> Option<(usize, usize, &'static str)> {
    let haystack = &text[from..];
    let mut best: Option<(usize, usize, &'static str)> = None;
    for &label in CONFIDENCE_LABELS {
        if let Some(off) = haystack.find(label) {
            let label_start = from + off;
            let label_end = label_start + label.len();
            if is_identifier_boundary(text, label_start, label_end) {
                pick_earliest(&mut best, (label_start, label_end, label));
            }
        }
        // `— label` form: match the literal " — label" with an em dash.
        if let Some(off) = find_label_with_affixes(haystack, " — ", label, "") {
            // Highlight just the label, not the em-dash separator.
            let label_start = from + off + " — ".len();
            let label_end = label_start + label.len();
            if is_identifier_boundary(text, label_start, label_end) {
                pick_earliest(&mut best, (label_start, label_end, label));
            }
        }
        if let Some(off) = find_label_with_affixes(haystack, "[", label, "]") {
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

fn find_label_with_affixes(
    haystack: &str,
    prefix: &str,
    label: &str,
    suffix: &str,
) -> Option<usize> {
    let mut cursor = 0;
    while let Some(off) = haystack[cursor..].find(prefix) {
        let candidate = cursor + off;
        let label_start = candidate + prefix.len();
        let suffix_start = label_start + label.len();
        let suffix_end = suffix_start + suffix.len();
        if haystack.get(label_start..suffix_start) == Some(label)
            && haystack.get(suffix_start..suffix_end) == Some(suffix)
        {
            return Some(candidate);
        }
        cursor = label_start;
    }
    None
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

fn is_identifier_boundary(text: &str, start: usize, end: usize) -> bool {
    !text
        .as_bytes()
        .get(start.saturating_sub(1))
        .filter(|_| start > 0)
        .is_some_and(is_identifier_byte)
        && !text.as_bytes().get(end).is_some_and(is_identifier_byte)
}

fn is_identifier_byte(byte: &u8) -> bool {
    byte.is_ascii_alphanumeric() || *byte == b'_'
}

fn display_link_url(url: &str) -> String {
    middle_truncate_chars(url, MAX_LINK_URL_CHARS)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out = String::new();
    for _ in 0..max_chars {
        let Some(ch) = chars.next() else {
            return text.to_string();
        };
        out.push(ch);
    }
    if chars.next().is_some() && max_chars >= 3 {
        out.truncate(out.len().saturating_sub(3));
        out.push_str("...");
    }
    out
}

fn middle_truncate_chars(text: &str, max_chars: usize) -> String {
    let len = text.chars().count();
    if len <= max_chars {
        return text.to_string();
    }
    if max_chars < 5 {
        return truncate_chars(text, max_chars);
    }
    let marker = "...";
    let keep = max_chars - marker.len();
    let head = keep / 2;
    let tail = keep - head;
    let prefix_end = if head == 0 {
        0
    } else {
        text.char_indices()
            .nth(head)
            .map(|(idx, _)| idx)
            .unwrap_or(text.len())
    };
    let suffix_start = if tail == 0 {
        text.len()
    } else {
        text.char_indices()
            .nth(len - tail)
            .map(|(idx, _)| idx)
            .unwrap_or(text.len())
    };
    let mut out =
        String::with_capacity(prefix_end + marker.len() + text.len().saturating_sub(suffix_start));
    out.push_str(&text[..prefix_end]);
    out.push_str(marker);
    out.push_str(&text[suffix_start..]);
    out
}

fn compact_unbroken_text_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut token = String::new();
    for ch in text.chars() {
        if ch.is_whitespace() {
            flush_compact_token(&mut out, &mut token);
            out.push(ch);
        } else {
            token.push(ch);
        }
    }
    flush_compact_token(&mut out, &mut token);
    out
}

fn flush_compact_token(out: &mut String, token: &mut String) {
    if token.is_empty() {
        return;
    }
    let char_count = token.chars().count();
    let has_break_points = token.contains('/') || token.contains('\\');
    if char_count > MAX_UNBROKEN_TEXT_TOKEN_CHARS && !has_break_points {
        out.push_str(&middle_truncate_chars(token, MAX_UNBROKEN_TEXT_TOKEN_CHARS));
    } else {
        out.push_str(token);
    }
    token.clear();
}

fn collapse_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            pending_space = true;
        } else {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            out.push(ch);
            pending_space = false;
        }
    }
    out
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
