//! Streaming previewer for `apply_patch` tool arguments.
//!
//! Provider adapters can route `tool_call_arguments_delta` events
//! straight into [`JsonPatchPreviewParser::push_delta`] as the model
//! emits them. The parser is push-driven and emits two flavours of
//! events:
//!
//! * [`PatchPreviewEvent::Partial`] — best-effort snapshots of the
//!   `path`/`search`/`replace` fields for the patch object currently
//!   being streamed. Surfaces every time a tracked string field
//!   finishes streaming so the TUI can paint a streaming diff frame-
//!   by-frame via [`render_streaming_preview`].
//! * [`PatchPreviewEvent::Patch`] / [`PatchPreviewEvent::Complete`] —
//!   structural events that fire once a patch object (or the whole
//!   `patches` array) closes. Carry short content hashes so the TUI
//!   can tell distinct patches apart even when several stream back-to-
//!   back.
//!
//! The provisional preview is informational only — the final approval
//! prompt is still driven by the fully-decoded tool args, and nothing
//! in this module ever triggers an apply.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::render::diff;
use crate::render::palette::AMBER;

/// Best-effort snapshot of the fields parsed from an in-flight patch
/// object inside `apply_patch`'s `patches` array.
///
/// Only fields whose closing `"` has already streamed are populated;
/// the rest stay `None` until later chunks land.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatchPartial {
    /// Index of this patch in the `patches` array.
    pub index: usize,
    pub path: Option<String>,
    pub search: Option<String>,
    pub replace: Option<String>,
}

/// Incremental events the TUI can paint while patch JSON is still streaming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchPreviewEvent {
    /// A best-effort partial snapshot for the patch object currently
    /// being streamed. Emitted whenever a tracked string field finishes
    /// streaming with new content; consumers should treat the latest
    /// `Partial` as the freshest preview state.
    Partial(PatchPartial),
    /// A single patch object has finished parsing inside the `patches` array.
    Patch {
        index: usize,
        path: String,
        search_hash: String,
        replace_hash: String,
    },
    /// The `patches` array has closed; no further patch objects will appear.
    Complete { count: usize },
}

/// Push-driven JSON parser that recognises
/// `{"patches": [ { ... }, { ... }, ... ]}` shaped tool-call arguments and
/// emits a [`PatchPreviewEvent::Partial`] each time a tracked field
/// (`path`, `search`, `replace`) finishes streaming inside the current
/// patch object, plus a [`PatchPreviewEvent::Patch`] when the object
/// closes.
///
/// Escaped quotes inside string literals are handled at byte level, so a
/// `search` or `replace` body containing `\"` does not falsely close a
/// sub-string.
#[derive(Debug, Default)]
pub struct JsonPatchPreviewParser {
    state: ParserState,
    buf: String,
    object_start: Option<usize>,
    in_string: bool,
    escape: bool,
    depth: usize,
    patches_array_depth: Option<usize>,
    emitted: usize,
    completed: bool,
    /// Best-effort snapshot for the patch object currently being parsed
    /// (or the most recently closed one). Reset when a new patch object
    /// opens.
    current_partial: PatchPartial,
    /// Snapshot of `current_partial` at the most recent `Partial`
    /// emission. Used to suppress duplicate events when a string close
    /// didn't actually surface new tracked content.
    last_emitted_partial: PatchPartial,
}

#[derive(Debug, Default)]
enum ParserState {
    #[default]
    Searching,
    AwaitingPatchesArray,
    InsidePatchesArray,
    Done,
}

#[derive(Deserialize)]
struct PatchShape {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    search: Option<String>,
    #[serde(default)]
    replace: Option<String>,
}

impl JsonPatchPreviewParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the next chunk of streamed tool-call arguments. Returns
    /// every event that became known after this delta — typically zero
    /// or one [`PatchPreviewEvent::Partial`] per tracked-field close,
    /// plus [`PatchPreviewEvent::Patch`] / [`PatchPreviewEvent::Complete`]
    /// at structural boundaries.
    pub fn push_delta(&mut self, delta: &str) -> Vec<PatchPreviewEvent> {
        let mut events = Vec::new();
        for byte in delta.bytes() {
            self.push_byte(byte, &mut events);
        }
        events
    }

    /// Signal end-of-stream. Emits a `Complete` event if the parser saw a
    /// `patches` array open but never saw the matching close (truncated JSON,
    /// canceled call, etc.) — the count reflects however many patch objects
    /// completed before truncation.
    pub fn finish(&mut self) -> Vec<PatchPreviewEvent> {
        let mut events = Vec::new();
        if !self.completed && self.patches_array_depth.is_some() {
            events.push(PatchPreviewEvent::Complete {
                count: self.emitted,
            });
            self.completed = true;
            self.state = ParserState::Done;
        }
        events
    }

    /// Number of patch events emitted so far.
    pub fn emitted_count(&self) -> usize {
        self.emitted
    }

    /// Best-effort latest snapshot for the patch currently being
    /// streamed (or the most recent one when the patch object has just
    /// closed). Returns an empty snapshot until the first tracked field
    /// closes.
    pub fn latest_partial(&self) -> &PatchPartial {
        &self.current_partial
    }

    fn push_byte(&mut self, byte: u8, events: &mut Vec<PatchPreviewEvent>) {
        if matches!(self.state, ParserState::Done) {
            return;
        }
        let ch = byte as char;
        self.buf.push(ch);
        if self.in_string {
            if self.escape {
                self.escape = false;
            } else if ch == '\\' {
                self.escape = true;
            } else if ch == '"' {
                self.in_string = false;
                if self.object_start.is_some()
                    && matches!(self.state, ParserState::InsidePatchesArray)
                {
                    self.refresh_partial(events);
                }
            }
            return;
        }
        match ch {
            '"' => {
                self.in_string = true;
            }
            '{' | '[' => {
                self.depth += 1;
                if matches!(self.state, ParserState::AwaitingPatchesArray) && ch == '[' {
                    self.state = ParserState::InsidePatchesArray;
                    self.patches_array_depth = Some(self.depth);
                } else if matches!(self.state, ParserState::InsidePatchesArray)
                    && ch == '{'
                    && self.object_start.is_none()
                    && self.patches_array_depth == Some(self.depth - 1)
                {
                    // First byte inside a new top-level patch object —
                    // record the position so we can extract the full
                    // JSON when the matching '}' arrives, and reset the
                    // partial snapshot so an earlier patch's fields
                    // don't leak into the new one.
                    self.object_start = Some(self.buf.len() - 1);
                    self.current_partial = PatchPartial {
                        index: self.emitted,
                        ..Default::default()
                    };
                    self.last_emitted_partial = self.current_partial.clone();
                }
            }
            '}' | ']' => {
                if matches!(self.state, ParserState::InsidePatchesArray)
                    && ch == '}'
                    && let Some(start) = self.object_start
                    && self.patches_array_depth == Some(self.depth - 1)
                {
                    let slice = &self.buf[start..self.buf.len()];
                    if let Ok(shape) = serde_json::from_str::<PatchShape>(slice) {
                        let path = shape.path.unwrap_or_default();
                        let search_hash = sha256_short(shape.search.as_deref().unwrap_or_default());
                        let replace_hash =
                            sha256_short(shape.replace.as_deref().unwrap_or_default());
                        events.push(PatchPreviewEvent::Patch {
                            index: self.emitted,
                            path,
                            search_hash,
                            replace_hash,
                        });
                        self.emitted += 1;
                    }
                    self.object_start = None;
                }
                if self.depth == 0 {
                    // Unbalanced — bail out.
                    self.state = ParserState::Done;
                    return;
                }
                self.depth -= 1;
                if ch == ']'
                    && matches!(self.state, ParserState::InsidePatchesArray)
                    && Some(self.depth) == self.patches_array_depth.map(|d| d - 1)
                {
                    events.push(PatchPreviewEvent::Complete {
                        count: self.emitted,
                    });
                    self.completed = true;
                    self.state = ParserState::Done;
                }
            }
            _ => {}
        }
        // Keyword detection: look for `"patches"` followed by a `:` before any
        // unrelated structural token. We do this by scanning the recent
        // suffix; cheap because the buffer is bounded by the streamed args.
        if matches!(self.state, ParserState::Searching) && ends_with_patches_key(&self.buf) {
            self.state = ParserState::AwaitingPatchesArray;
        }
    }

    fn refresh_partial(&mut self, events: &mut Vec<PatchPreviewEvent>) {
        let Some(start) = self.object_start else {
            return;
        };
        let slice = &self.buf[start..self.buf.len()];
        let extracted = extract_partial_fields(slice);
        self.current_partial = PatchPartial {
            index: self.emitted,
            path: extracted.path,
            search: extracted.search,
            replace: extracted.replace,
        };
        if self.current_partial != self.last_emitted_partial {
            events.push(PatchPreviewEvent::Partial(self.current_partial.clone()));
            self.last_emitted_partial = self.current_partial.clone();
        }
    }
}

/// Render the streaming preview diff for a partial patch snapshot.
///
/// Synthesizes a unified-diff-style body from `search` (rendered as `-`
/// lines) and `replace` (rendered as `+` lines), then runs the result
/// through the production [`crate::render::diff::render_patch_full_lines`]
/// path so the gutter, sign colours, and syntax highlighting match the
/// final approval-prompt diff exactly.
///
/// Callers can invoke this on every `Partial` event to repaint the diff
/// frame-by-frame as new chunks arrive; the rendered output is purely
/// informational and never authorises an apply.
pub fn render_streaming_preview(partial: &PatchPartial) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(path) = &partial.path {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("✎ {path}"),
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  (streaming preview)",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    let mut diff_text = String::new();
    if let Some(search) = partial.search.as_deref() {
        append_diff_lines(&mut diff_text, '-', search);
    }
    if let Some(replace) = partial.replace.as_deref() {
        append_diff_lines(&mut diff_text, '+', replace);
    }
    if !diff_text.is_empty() {
        let hint = partial
            .path
            .as_deref()
            .and_then(diff::language_hint_from_path);
        for mut line in diff::render_patch_full_lines(&diff_text, hint) {
            line.spans.insert(0, Span::raw("  "));
            lines.push(line);
        }
    }
    lines
}

fn append_diff_lines(buf: &mut String, sign: char, body: &str) {
    if body.is_empty() {
        return;
    }
    let body_ends_with_newline = body.ends_with('\n');
    let mut parts = body.split('\n').peekable();
    while let Some(line) = parts.next() {
        let is_last = parts.peek().is_none();
        if is_last && line.is_empty() && body_ends_with_newline {
            // Trailing `\n` produces a phantom empty element; suppress it
            // so the diff body isn't padded with a sign-only line.
            break;
        }
        buf.push(sign);
        buf.push_str(line);
        buf.push('\n');
    }
}

fn ends_with_patches_key(buf: &str) -> bool {
    // Match `"patches":` (optionally with whitespace before `:`). We scan from
    // the tail to keep this O(1) per byte.
    let trimmed = buf.trim_end();
    let trimmed = trimmed.trim_end_matches(':');
    let trimmed = trimmed.trim_end();
    trimmed.ends_with("\"patches\"")
}

fn sha256_short(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let bytes = hasher.finalize();
    // Short prefix: enough to distinguish previews, cheap to render.
    let mut out = String::with_capacity(16);
    for byte in bytes.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Tolerant tokenizer that walks a (possibly truncated) JSON object
/// slice and pulls out the `path`, `search`, and `replace` string
/// fields when they have a properly closed string value. Nested
/// objects/arrays and other field types are skipped so the parser
/// stays robust against schema additions and intermediate truncation.
fn extract_partial_fields(slice: &str) -> PatchPartial {
    let mut partial = PatchPartial::default();
    let mut chars = slice.chars().peekable();
    if chars.next() != Some('{') {
        return partial;
    }
    loop {
        skip_object_separators(&mut chars);
        match chars.peek() {
            None | Some('}') => break,
            Some('"') => {
                chars.next();
            }
            _ => break,
        }
        let key = match read_json_string(&mut chars) {
            Some((key, true)) => key,
            _ => break,
        };
        skip_whitespace(&mut chars);
        match chars.next() {
            Some(':') => {}
            _ => break,
        }
        skip_whitespace(&mut chars);
        match chars.peek() {
            Some('"') => {
                chars.next();
                if let Some((value, true)) = read_json_string(&mut chars) {
                    match key.as_str() {
                        "path" => partial.path = Some(value),
                        "search" => partial.search = Some(value),
                        "replace" => partial.replace = Some(value),
                        _ => {}
                    }
                } else {
                    // Value string truncated mid-stream; stop here so
                    // we don't surface a partial mid-string body.
                    break;
                }
            }
            Some('{') | Some('[') => {
                if !skip_nested(&mut chars) {
                    break;
                }
            }
            None => break,
            _ => {
                skip_primitive(&mut chars);
            }
        }
    }
    partial
}

fn skip_object_separators(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(&c) = chars.peek() {
        if matches!(c, ' ' | '\t' | '\n' | '\r' | ',') {
            chars.next();
        } else {
            break;
        }
    }
}

fn skip_whitespace(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
}

/// Read a JSON string body up to (and including) the closing `"`.
/// Returns `(text, closed)` where `closed` is `true` only when the
/// closing quote was consumed; on a truncated stream we still hand back
/// whatever we collected so the caller can decide what to do with it.
fn read_json_string(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Option<(String, bool)> {
    let mut out = String::new();
    let mut escape = false;
    while let Some(c) = chars.next() {
        if escape {
            match c {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\x08'),
                'f' => out.push('\x0C'),
                'u' => {
                    let mut hex = String::with_capacity(4);
                    for _ in 0..4 {
                        match chars.next() {
                            Some(h) => hex.push(h),
                            None => return Some((out, false)),
                        }
                    }
                    if let Ok(code) = u32::from_str_radix(&hex, 16)
                        && let Some(ch) = char::from_u32(code)
                    {
                        out.push(ch);
                    }
                }
                other => out.push(other),
            }
            escape = false;
        } else if c == '\\' {
            escape = true;
        } else if c == '"' {
            return Some((out, true));
        } else {
            out.push(c);
        }
    }
    Some((out, false))
}

fn skip_nested(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;
    for c in chars.by_ref() {
        if in_string {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn skip_primitive(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(&c) = chars.peek() {
        if matches!(c, ',' | '}' | ']') {
            return;
        }
        chars.next();
    }
}
