//! Self-contained HTML export of a Squeezy session.
//!
//! Produces a single `.html` document — inline `<style>` block, no external
//! CSS / JS / fonts — that renders the session's user / assistant /
//! tool exchange. Tool output runs through a small ANSI → HTML translator
//! so terminal color sequences come out as inline-styled spans, and all
//! user / model / tool content is HTML-escaped before reaching the page.
//!
//! The pipeline is escape → ANSI parser → per-event render → wrap in a
//! static document, driven directly by Squeezy's [`SessionRecord`] event
//! stream. The output is intentionally portable: a static `.html` a user
//! can email, attach to a bug report, or open offline without depending
//! on any SaaS service.

use std::fmt::Write;

use serde_json::Value;
use squeezy_store::{SessionEventKind, SessionRecord};

/// Options controlling [`export_session_to_html`].
#[derive(Debug, Clone)]
pub struct ExportOpts {
    /// When `true` (the default) tool calls and tool outputs are rendered
    /// inline. Set to `false` to ship only the conversational thread —
    /// useful when the tool output is large or noisy and the reader only
    /// needs the user ↔ assistant exchange.
    pub include_tool_outputs: bool,
    /// Light or dark color scheme. Both are self-contained; switching
    /// after export is a one-attribute change a user can make by hand.
    pub theme: ExportTheme,
}

impl Default for ExportOpts {
    fn default() -> Self {
        Self {
            include_tool_outputs: true,
            theme: ExportTheme::Dark,
        }
    }
}

/// Visual theme for the rendered document. The export bakes the theme
/// into the inline `<style>` block so the file stays self-contained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportTheme {
    Light,
    Dark,
}

impl ExportTheme {
    fn body_class(self) -> &'static str {
        match self {
            Self::Light => "theme-light",
            Self::Dark => "theme-dark",
        }
    }
}

/// Errors produced while building the HTML document. The current
/// failure modes are `std::fmt::Write` errors against the in-memory
/// `String` buffer (effectively unreachable unless allocation fails),
/// but the dedicated error type keeps the surface ready for future
/// failure modes like template-load errors without breaking callers.
#[derive(Debug)]
pub enum ExportError {
    /// A `write!` into the in-memory HTML buffer failed.
    Format(std::fmt::Error),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Format(error) => write!(f, "html format error: {error}"),
        }
    }
}

impl std::error::Error for ExportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Format(error) => Some(error),
        }
    }
}

impl From<std::fmt::Error> for ExportError {
    fn from(value: std::fmt::Error) -> Self {
        Self::Format(value)
    }
}

/// Render `record` to a self-contained HTML document.
///
/// The output contains no `<script>` tags and no external resources;
/// every byte the browser needs to lay out the page lives between the
/// returned string's first and last byte. Caller is responsible for
/// writing it to disk (CLI: `--output path`).
pub fn export_session_to_html(
    record: &SessionRecord,
    opts: &ExportOpts,
) -> Result<String, ExportError> {
    let mut out = String::with_capacity(8 * 1024);
    writeln!(out, "<!DOCTYPE html>\n<html lang=\"en\">\n<head>")?;
    writeln!(out, "  <meta charset=\"utf-8\">")?;
    writeln!(
        out,
        "  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">"
    )?;
    writeln!(
        out,
        "  <title>Squeezy session {}</title>",
        escape_html(&record.metadata.session_id)
    )?;
    writeln!(out, "  <style>{}</style>", inline_css())?;
    writeln!(out, "</head>")?;
    writeln!(out, "<body class=\"{}\">", opts.theme.body_class())?;
    writeln!(out, "  <main class=\"session\">")?;

    write_header(&mut out, record)?;
    write_messages(&mut out, record, opts)?;
    write_footer(&mut out, record)?;

    writeln!(out, "  </main>\n</body>\n</html>")?;
    Ok(out)
}

fn write_header(out: &mut String, record: &SessionRecord) -> Result<(), ExportError> {
    let meta = &record.metadata;
    writeln!(out, "    <header class=\"session-header\">")?;
    writeln!(out, "      <h1>Squeezy session</h1>")?;
    writeln!(out, "      <dl class=\"meta\">")?;
    write_meta_row(out, "session", &meta.session_id)?;
    write_meta_row(out, "provider", &meta.provider)?;
    write_meta_row(out, "model", &meta.model)?;
    write_meta_row(out, "mode", meta.mode.as_str())?;
    write_meta_row(out, "status", meta.status.as_str())?;
    if let Some(branch) = meta.branch.as_deref() {
        write_meta_row(out, "branch", branch)?;
    }
    if let Some(repo) = meta.repo_root.as_deref() {
        write_meta_row(out, "repo", repo)?;
    }
    write_meta_row(out, "started_ms", &meta.started_at_ms.to_string())?;
    if let Some(ended) = meta.ended_at_ms {
        write_meta_row(out, "ended_ms", &ended.to_string())?;
    }
    writeln!(out, "      </dl>")?;
    writeln!(out, "    </header>")?;
    Ok(())
}

fn write_meta_row(out: &mut String, key: &str, value: &str) -> Result<(), ExportError> {
    writeln!(
        out,
        "        <dt>{}</dt><dd>{}</dd>",
        escape_html(key),
        escape_html(value)
    )?;
    Ok(())
}

fn write_messages(
    out: &mut String,
    record: &SessionRecord,
    opts: &ExportOpts,
) -> Result<(), ExportError> {
    writeln!(out, "    <ol class=\"messages\">")?;
    // Tool-call → tool-result pairing: pi groups them under one card.
    // We keep the same shape so a user reading the export sees the args
    // and the output side-by-side, not as two unrelated entries.
    let mut pending_calls: std::collections::HashMap<String, PendingCall> =
        std::collections::HashMap::new();
    for event in &record.events {
        let Some(kind) = SessionEventKind::try_from_event(event) else {
            continue;
        };
        match kind {
            SessionEventKind::UserMessage { text } => {
                write_user(out, &text)?;
            }
            SessionEventKind::AssistantCompleted { text, .. } => {
                if text.is_empty() {
                    continue;
                }
                write_assistant(out, &text)?;
            }
            SessionEventKind::ToolCall {
                call_id,
                tool,
                arguments,
            } => {
                if !opts.include_tool_outputs {
                    continue;
                }
                if call_id.is_empty() {
                    write_tool_call(out, &tool, &arguments, None)?;
                } else {
                    pending_calls.insert(call_id.clone(), PendingCall { tool, arguments });
                }
            }
            SessionEventKind::ToolResult { output } => {
                if !opts.include_tool_outputs {
                    continue;
                }
                let call_id = output
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let body = output
                    .get("output")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| pretty_json(&output));
                let matched = call_id.as_ref().and_then(|id| pending_calls.remove(id));
                if let Some(call) = matched {
                    write_tool_call(out, &call.tool, &call.arguments, Some(&body))?;
                } else {
                    write_tool_result_only(out, call_id.as_deref(), &body)?;
                }
            }
            SessionEventKind::Reasoning { .. } => {
                // Reasoning blobs are provider-specific encrypted /
                // signed snapshots used for resume. They are not the
                // model's user-visible chain-of-thought, so we
                // deliberately do not surface them in the export.
            }
            SessionEventKind::ContextCompacted { summary, .. } => {
                write_lifecycle(out, "context compacted", summary.as_deref().unwrap_or(""))?;
            }
            SessionEventKind::ApprovalRequested { tool, .. } => {
                write_lifecycle(out, "approval requested", &tool)?;
            }
            SessionEventKind::ApprovalDecided { tool, decision, .. } => {
                write_lifecycle(out, "approval decided", &format!("{tool}: {decision}"))?;
            }
            SessionEventKind::SessionStarted => {}
            SessionEventKind::SessionEnded { status } => {
                write_lifecycle(out, "session ended", &status)?;
            }
            SessionEventKind::SessionResumed => {
                write_lifecycle(out, "session resumed", "")?;
            }
            SessionEventKind::Cancelled => {
                write_lifecycle(out, "cancelled", "")?;
            }
            SessionEventKind::Failed { error } => {
                write_lifecycle(out, "failed", &error)?;
            }
            SessionEventKind::Custom { .. } => {
                // Extension-authored sidecar data: the export is a
                // user-facing transcript, not a debug dump, so we skip
                // these silently rather than surfacing arbitrary
                // extension payloads to the reader.
            }
            SessionEventKind::Unknown => {
                // Skip silently: an Unknown variant came from a future
                // Squeezy version writing an event kind this binary
                // does not understand. Surfacing it as an empty card
                // would just confuse the reader.
            }
        }
    }
    // Surface tool calls whose result never arrived (provider crashed
    // mid-turn, user cancelled, etc.) so the reader knows the call was
    // attempted even if the output is missing from the rendered card.
    for (_call_id, call) in pending_calls {
        write_tool_call(out, &call.tool, &call.arguments, None)?;
    }
    writeln!(out, "    </ol>")?;
    Ok(())
}

struct PendingCall {
    tool: String,
    arguments: Value,
}

fn write_user(out: &mut String, text: &str) -> Result<(), ExportError> {
    writeln!(out, "      <li class=\"msg msg-user\">")?;
    writeln!(out, "        <div class=\"role\">User</div>")?;
    writeln!(
        out,
        "        <div class=\"content\"><pre>{}</pre></div>",
        escape_html(text)
    )?;
    writeln!(out, "      </li>")?;
    Ok(())
}

fn write_assistant(out: &mut String, text: &str) -> Result<(), ExportError> {
    writeln!(out, "      <li class=\"msg msg-assistant\">")?;
    writeln!(out, "        <div class=\"role\">Assistant</div>")?;
    writeln!(
        out,
        "        <div class=\"content\"><pre>{}</pre></div>",
        escape_html(text)
    )?;
    writeln!(out, "      </li>")?;
    Ok(())
}

fn write_tool_call(
    out: &mut String,
    tool: &str,
    arguments: &Value,
    output: Option<&str>,
) -> Result<(), ExportError> {
    let args_pretty = pretty_json(arguments);
    writeln!(out, "      <li class=\"msg msg-tool\">")?;
    writeln!(
        out,
        "        <div class=\"role\">Tool: {}</div>",
        escape_html(tool)
    )?;
    writeln!(
        out,
        "        <details class=\"tool-args\"><summary>arguments</summary><pre>{}</pre></details>",
        escape_html(&args_pretty)
    )?;
    if let Some(body) = output {
        writeln!(
            out,
            "        <div class=\"tool-output\">{}</div>",
            ansi_lines_to_html(body)
        )?;
    }
    writeln!(out, "      </li>")?;
    Ok(())
}

fn write_tool_result_only(
    out: &mut String,
    call_id: Option<&str>,
    body: &str,
) -> Result<(), ExportError> {
    let label = call_id
        .map(|id| format!("Tool result · {id}"))
        .unwrap_or_else(|| "Tool result".to_string());
    writeln!(out, "      <li class=\"msg msg-tool\">")?;
    writeln!(
        out,
        "        <div class=\"role\">{}</div>",
        escape_html(&label)
    )?;
    writeln!(
        out,
        "        <div class=\"tool-output\">{}</div>",
        ansi_lines_to_html(body)
    )?;
    writeln!(out, "      </li>")?;
    Ok(())
}

fn write_lifecycle(out: &mut String, kind: &str, detail: &str) -> Result<(), ExportError> {
    writeln!(out, "      <li class=\"msg msg-lifecycle\">")?;
    writeln!(
        out,
        "        <div class=\"role\">{}</div>",
        escape_html(kind)
    )?;
    if !detail.is_empty() {
        writeln!(
            out,
            "        <div class=\"detail\">{}</div>",
            escape_html(detail)
        )?;
    }
    writeln!(out, "      </li>")?;
    Ok(())
}

fn write_footer(out: &mut String, record: &SessionRecord) -> Result<(), ExportError> {
    writeln!(out, "    <footer class=\"session-footer\">")?;
    writeln!(
        out,
        "      <p>Exported by Squeezy · {} event(s)</p>",
        record.metadata.event_count
    )?;
    writeln!(out, "    </footer>")?;
    Ok(())
}

/// HTML-escape `s` against attribute and content contexts. Covers the
/// standard five-entity set so a session containing `<script>`
/// literally renders as text.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    push_escaped_html(&mut out, s);
    out
}

fn push_escaped_html(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
}

fn pretty_json(value: &Value) -> String {
    if value.is_null() {
        return String::new();
    }
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

// ---- ANSI -> HTML ----------------------------------------------------------
//
// Standard 16-color ANSI palette + SGR dispatch so the rendered HTML
// matches a terminal as closely as possible. Pure Rust; no external
// dep.

/// Standard ANSI 16-color palette. Index 0-7 are the standard colors,
/// 8-15 are their bright variants.
const ANSI_COLORS: [&str; 16] = [
    "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0",
    "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct TextStyle {
    fg: Option<u32>,
    bg: Option<u32>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
}

impl TextStyle {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn is_empty(&self) -> bool {
        self.fg.is_none()
            && self.bg.is_none()
            && !self.bold
            && !self.dim
            && !self.italic
            && !self.underline
    }

    fn inline_css(self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(color) = self.fg {
            parts.push(format!("color:{}", color_hex(color)));
        }
        if let Some(color) = self.bg {
            parts.push(format!("background-color:{}", color_hex(color)));
        }
        if self.bold {
            parts.push("font-weight:bold".to_string());
        }
        if self.dim {
            parts.push("opacity:0.6".to_string());
        }
        if self.italic {
            parts.push("font-style:italic".to_string());
        }
        if self.underline {
            parts.push("text-decoration:underline".to_string());
        }
        parts.join(";")
    }
}

fn color_hex(rgb: u32) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (rgb >> 16) & 0xff,
        (rgb >> 8) & 0xff,
        rgb & 0xff,
    )
}

fn palette_color(index: u8) -> u32 {
    let hex = ANSI_COLORS[index as usize];
    u32::from_str_radix(&hex[1..], 16).unwrap_or(0)
}

fn color_256(index: u8) -> u32 {
    if index < 16 {
        return palette_color(index);
    }
    if index < 232 {
        let cube = index - 16;
        let r = cube / 36;
        let g = (cube % 36) / 6;
        let b = cube % 6;
        let to_component = |n: u8| -> u32 { if n == 0 { 0 } else { 55 + u32::from(n) * 40 } };
        return (to_component(r) << 16) | (to_component(g) << 8) | to_component(b);
    }
    let gray: u32 = 8 + u32::from(index - 232) * 10;
    (gray << 16) | (gray << 8) | gray
}

fn apply_sgr(params: &[u16], style: &mut TextStyle) {
    let mut i = 0;
    while i < params.len() {
        let code = params[i];
        match code {
            0 => style.reset(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            30..=37 => style.fg = Some(palette_color((code - 30) as u8)),
            38 => {
                if let Some(&5) = params.get(i + 1)
                    && let Some(&idx) = params.get(i + 2)
                {
                    style.fg = Some(color_256(idx as u8));
                    i += 2;
                } else if let Some(&2) = params.get(i + 1)
                    && let Some(&r) = params.get(i + 2)
                    && let Some(&g) = params.get(i + 3)
                    && let Some(&b) = params.get(i + 4)
                {
                    style.fg = Some(
                        ((r as u32 & 0xff) << 16) | ((g as u32 & 0xff) << 8) | (b as u32 & 0xff),
                    );
                    i += 4;
                }
            }
            39 => style.fg = None,
            40..=47 => style.bg = Some(palette_color((code - 40) as u8)),
            48 => {
                if let Some(&5) = params.get(i + 1)
                    && let Some(&idx) = params.get(i + 2)
                {
                    style.bg = Some(color_256(idx as u8));
                    i += 2;
                } else if let Some(&2) = params.get(i + 1)
                    && let Some(&r) = params.get(i + 2)
                    && let Some(&g) = params.get(i + 3)
                    && let Some(&b) = params.get(i + 4)
                {
                    style.bg = Some(
                        ((r as u32 & 0xff) << 16) | ((g as u32 & 0xff) << 8) | (b as u32 & 0xff),
                    );
                    i += 4;
                }
            }
            49 => style.bg = None,
            90..=97 => style.fg = Some(palette_color((code - 90 + 8) as u8)),
            100..=107 => style.bg = Some(palette_color((code - 100 + 8) as u8)),
            _ => {}
        }
        i += 1;
    }
}

/// Convert a single ANSI-bearing string to HTML. Text outside of ANSI
/// sequences is HTML-escaped; styled regions are wrapped in
/// `<span style="…">` so the rendered document needs no class table.
pub(crate) fn ansi_to_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut style = TextStyle::default();
    let mut in_span = false;
    let bytes = text.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == 0x1b
            && cursor + 1 < bytes.len()
            && bytes[cursor + 1] == b'['
            && let Some(end) = find_sgr_end(bytes, cursor + 2)
        {
            let params: Vec<u16> = std::str::from_utf8(&bytes[cursor + 2..end])
                .ok()
                .map(|raw| {
                    if raw.is_empty() {
                        vec![0]
                    } else {
                        raw.split(';')
                            .map(|part| part.parse::<u16>().unwrap_or(0))
                            .collect()
                    }
                })
                .unwrap_or_else(|| vec![0]);
            if in_span {
                out.push_str("</span>");
                in_span = false;
            }
            apply_sgr(&params, &mut style);
            if !style.is_empty() {
                out.push_str("<span style=\"");
                out.push_str(&style.inline_css());
                out.push_str("\">");
                in_span = true;
            }
            cursor = end + 1;
            continue;
        }
        // Push one *char* at a time so we don't bisect a multi-byte
        // UTF-8 sequence with the index-based byte cursor.
        let remaining = std::str::from_utf8(&bytes[cursor..]).unwrap_or("");
        if let Some(ch) = remaining.chars().next() {
            push_escaped_html(&mut out, &ch.to_string());
            cursor += ch.len_utf8();
        } else {
            cursor += 1;
        }
    }
    if in_span {
        out.push_str("</span>");
    }
    out
}

/// Walk `bytes` starting at `start` until we hit the SGR terminator
/// (`m`). Returns the index of the `m`, or `None` if the sequence is
/// malformed (no terminator before EOF). Only digits, `;`, and the
/// terminator are valid; anything else aborts so we treat the sequence
/// as raw text instead of silently swallowing input.
fn find_sgr_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'm' {
            return Some(i);
        }
        if !b.is_ascii_digit() && b != b';' {
            return None;
        }
        i += 1;
    }
    None
}

/// Convert ANSI-bearing tool output into the per-line `<div>` shape pi
/// uses. Each line keeps its own block so a CSS `white-space: pre`
/// rule on `.ansi-line` preserves indentation without forcing the
/// whole document into a `<pre>`.
fn ansi_lines_to_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    for line in text.split('\n') {
        let body = ansi_to_html(line);
        out.push_str("<div class=\"ansi-line\">");
        if body.is_empty() {
            // Empty line still needs to take up vertical space so the
            // whitespace is faithful — same trick pi uses.
            out.push_str("&nbsp;");
        } else {
            out.push_str(&body);
        }
        out.push_str("</div>");
    }
    out
}

/// The full CSS bundle baked into the export. Kept under 200 lines per
/// the implementation contract; both themes coexist as
/// attribute-selected variants so the exported file does not depend on
/// a theme picker. The dark accent palette matches Squeezy's TUI so the
/// document looks at home next to the rest of the agent's output.
fn inline_css() -> &'static str {
    include_str!("export_html_styles.css")
}

#[cfg(test)]
#[path = "export_html_tests.rs"]
mod tests;
