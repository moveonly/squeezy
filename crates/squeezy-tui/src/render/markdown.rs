use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::render::{highlight, palette};

pub fn render_markdown(source: &str) -> Vec<Line<'static>> {
    let mut writer = Writer::default();
    for event in Parser::new_ext(source, Options::empty()) {
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

impl Writer {
    fn event(&mut self, event: Event<'_>) {
        if self.code_block.is_some() {
            self.code_event(event);
            return;
        }

        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                self.push_text(&text, self.current_style);
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
                self.push_text(
                    if checked { "[x] " } else { "[ ] " },
                    Style::default().fg(palette::GOLD),
                );
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
            Tag::BlockQuote(_) => self.quote_depth += 1,
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
            Tag::Link { .. } | Tag::Image { .. } => {}
            Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::FootnoteDefinition(_)
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
            TagEnd::Link
            | TagEnd::Image
            | TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
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
        let marker = if let Some(list) = self.list_stack.last_mut() {
            if list.ordered {
                let marker = format!("{}. ", list.next);
                list.next += 1;
                marker
            } else {
                "- ".to_string()
            }
        } else {
            "- ".to_string()
        };
        self.push_text(&marker, Style::default().fg(palette::GOLD));
    }

    fn push_style(&mut self, style: Style) {
        self.style_stack.push(self.current_style);
        self.current_style = self.current_style.patch(style);
    }

    fn pop_style(&mut self) {
        self.current_style = self.style_stack.pop().unwrap_or_default();
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
    let heading_color = match palette::palette_tone() {
        palette::PaletteTone::Dark => palette::GOLD,
        palette::PaletteTone::Light => palette::best_color((92, 65, 12)),
    };
    let mut style = Style::default()
        .fg(heading_color)
        .add_modifier(Modifier::BOLD);
    if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

fn inline_code_style() -> Style {
    Style::default().fg(Color::Cyan)
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
