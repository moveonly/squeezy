//! Semantic COPY/EXPORT substrate for the transcript surface.
//!
//! This module turns the shared transcript ROW MODEL
//! ([`crate::transcript_surface::TranscriptRow`]) plus a focus cursor into a
//! clipboard- or file-ready payload for a chosen *semantic unit*
//! ([`CopyScope`]) rendered in a chosen *format* ([`CopyFormat`]). It owns the
//! scope-resolution and formatting logic; the crate root (`lib.rs`) owns the
//! side effects (clipboard write, file write, status toast) and supplies the
//! row list, the focus `RowId`, and an `is_assistant` role predicate.
//!
//! ## BY-SURFACE caveat (read this before trusting a bulk copy)
//!
//! The rows fed here come from
//! [`crate::transcript_surface::build_transcript_rows`], which is built on
//! [`crate::wrap_entries`] — the **overlay** transcript pipeline, NOT the main
//! render path ([`crate::transcript_lines_for_render`]). For COPY this is
//! correct for *text*: the divergences between the two surfaces are styling and
//! animation (the main path adds `Tinted` tool cards, the settle-fold height
//! animation, the startup card, and a different turn divider), and the copy
//! substrate works off the plain `copy_text` projection, which drops styling
//! entirely. So:
//!
//! * **Entry-scoped copies** ([`CopyScope::FocusedEntry`],
//!   [`CopyScope::LastAssistant`], [`CopyScope::CurrentToolOutput`],
//!   [`CopyScope::CodeBlockUnderCursor`]) read only `entry_id`-owned rows and
//!   are already main-view-faithful — chrome never enters them.
//! * **Bulk copies** ([`CopyScope::Viewport`], [`CopyScope::FullTranscript`])
//!   reflect the *overlay* row model's chrome (its title banner / turn
//!   divider), which differs cosmetically from the main view. Fully
//!   main-view-faithful bulk copy awaits the `wrap_entries`-by-surface
//!   unification (parallelization-plan Phase 4); until then the difference is
//!   chrome text only, never message/tool/code content.
//!
//! ## Gutter stripping
//!
//! Every row's `copy_text` may carry a leading rail gutter (`│ ├ ╰─` plus the
//! node marker). All formats strip it via
//! [`crate::transcript_surface::strip_gutter`] so a paste begins at the first
//! content character — this is the strip the historical `copy_range` TODO
//! deferred.

use crate::transcript_surface::{EntryId, RowId, RowKind, TranscriptRow, strip_gutter};

/// The semantic unit a copy/export targets. The row model owns resolving each
/// of these to a concrete row subset (see [`resolve_scope`]); `lib.rs` only
/// chooses the scope and wires the side effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyScope {
    /// The current/focused entry: every row sharing the focused row's
    /// `entry_id`. When focus lands on chrome, falls back to the nearest
    /// preceding entry-owned row; when there is no focus, defaults to the live
    /// tail (the last entry-owned row).
    FocusedEntry,
    /// The last assistant message entry (kind == Message AND role == Assistant).
    LastAssistant,
    /// The focused tool-result entry, or the nearest `ToolResult` at/above
    /// focus.
    CurrentToolOutput,
    /// The fenced code block bracketing the focus row (interior only, fences
    /// excluded).
    CodeBlockUnderCursor,
    /// Every row currently visible in the main viewport.
    Viewport,
    /// Every row in the transcript.
    FullTranscript,
}

impl CopyScope {
    /// Short human label for the status/toast line ("copied <label>").
    pub(crate) fn label(self) -> &'static str {
        match self {
            CopyScope::FocusedEntry => "entry",
            CopyScope::LastAssistant => "assistant message",
            CopyScope::CurrentToolOutput => "tool output",
            CopyScope::CodeBlockUnderCursor => "code block",
            CopyScope::Viewport => "viewport",
            CopyScope::FullTranscript => "transcript",
        }
    }
}

/// How a resolved row subset is rendered into the payload string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyFormat {
    /// Plain text: gutter-stripped `copy_text`, one row per line.
    Plain,
    /// Markdown: gutter-stripped text with each message entry prefixed by a
    /// `**Assistant**` / `**User**` heading. Existing ``` fence rows are passed
    /// through verbatim (and suppress headings while open); the formatter does
    /// not synthesize or re-fence code blocks.
    Markdown,
    /// A JSON array of event objects, one per resolved entry:
    /// `{ "id", "kind", "text" }`. Chrome rows are skipped.
    JsonSlice,
}

impl CopyFormat {
    /// Default copy format. Plain text, the universally paste-safe choice.
    pub(crate) const fn default_format() -> Self {
        CopyFormat::Plain
    }

    /// Parse the `/export` format token (`md`/`markdown`, `txt`/`text`/`plain`,
    /// `json`). Case-insensitive. Returns `None` for anything else so the
    /// caller can surface a usage error.
    pub(crate) fn from_token(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "md" | "markdown" => Some(CopyFormat::Markdown),
            "txt" | "text" | "plain" => Some(CopyFormat::Plain),
            "json" => Some(CopyFormat::JsonSlice),
            _ => None,
        }
    }

    /// Conventional file extension for this format, for `/export`'s default
    /// filename.
    pub(crate) fn file_extension(self) -> &'static str {
        match self {
            CopyFormat::Plain => "txt",
            CopyFormat::Markdown => "md",
            CopyFormat::JsonSlice => "json",
        }
    }
}

// ---------------------------------------------------------------------------
// Scope resolution
// ---------------------------------------------------------------------------

/// Resolve `scope` to the concrete `RowId` range (inclusive) of `rows` it
/// covers, given a `focus` row and an `is_assistant` predicate over entry ids.
/// Returns `None` when the scope resolves to nothing (e.g. "copy tool output"
/// with no tool result, or "code block" with the cursor outside any fence).
///
/// `viewport` is `Some((from, to))` (inclusive `RowId`s) for [`CopyScope::Viewport`],
/// pre-computed by the caller from the live viewport geometry; ignored for
/// other scopes.
pub(crate) fn resolve_scope(
    rows: &[TranscriptRow],
    focus: Option<RowId>,
    scope: CopyScope,
    is_assistant: &dyn Fn(EntryId) -> bool,
    viewport: Option<(RowId, RowId)>,
) -> Option<(usize, usize)> {
    if rows.is_empty() {
        return None;
    }
    let last = rows.len() - 1;
    // Resolve the focus index, defaulting to the last entry-owned row (the live
    // tail) so entry/tool/code copies are useful before any selection lands.
    let focus_idx = focus
        .map(|r| r.0.min(last))
        .unwrap_or_else(|| last_entry_owned_index(rows).unwrap_or(last));

    match scope {
        CopyScope::FocusedEntry => resolve_focused_entry(rows, focus_idx),
        CopyScope::LastAssistant => resolve_last_assistant(rows, is_assistant),
        CopyScope::CurrentToolOutput => resolve_current_tool_output(rows, focus_idx),
        CopyScope::CodeBlockUnderCursor => resolve_code_block(rows, focus_idx),
        CopyScope::Viewport => viewport
            .map(|(from, to)| {
                let lo = from.0.min(last);
                let hi = to.0.min(last);
                (lo.min(hi), lo.max(hi))
            })
            .or(Some((0, last))),
        CopyScope::FullTranscript => Some((0, last)),
    }
}

/// Index of the last row owned by an entry (skipping trailing chrome like the
/// live pending tail), or `None` when the whole list is chrome.
fn last_entry_owned_index(rows: &[TranscriptRow]) -> Option<usize> {
    rows.iter().rposition(|r| r.entry_id.is_some())
}

/// The contiguous run of rows sharing the focused row's `entry_id`. When the
/// focus row is chrome (`entry_id == None`), fall back to the nearest preceding
/// entry-owned row and copy *its* entry. Returns `None` only when there is no
/// entry-owned row at or before focus.
fn resolve_focused_entry(rows: &[TranscriptRow], focus_idx: usize) -> Option<(usize, usize)> {
    let target = match rows[focus_idx].entry_id {
        Some(id) => id,
        None => {
            // Walk back to the nearest entry-owned row.
            let prev = rows[..=focus_idx]
                .iter()
                .rposition(|r| r.entry_id.is_some())?;
            rows[prev].entry_id?
        }
    };
    entry_run(rows, target)
}

/// The inclusive index range of every row whose `entry_id == target`. Because a
/// single entry's wrapped rows are contiguous in the row list, this is one run.
fn entry_run(rows: &[TranscriptRow], target: EntryId) -> Option<(usize, usize)> {
    let lo = rows.iter().position(|r| r.entry_id == Some(target))?;
    let hi = rows.iter().rposition(|r| r.entry_id == Some(target))?;
    Some((lo, hi))
}

/// The rows of the last assistant Message entry. Uses the `is_assistant`
/// predicate (role lives on the entry, not the row) to pick the last Message
/// entry whose role is Assistant.
fn resolve_last_assistant(
    rows: &[TranscriptRow],
    is_assistant: &dyn Fn(EntryId) -> bool,
) -> Option<(usize, usize)> {
    let target = rows
        .iter()
        .rev()
        .filter(|r| r.entry_kind == Some(RowKind::Message))
        .find_map(|r| r.entry_id.filter(|id| is_assistant(*id)))?;
    entry_run(rows, target)
}

/// The rows of the focused tool-result entry, or the nearest `ToolResult`
/// entry at/above focus.
fn resolve_current_tool_output(rows: &[TranscriptRow], focus_idx: usize) -> Option<(usize, usize)> {
    // Prefer the focused entry when it is itself a tool result.
    if rows[focus_idx].entry_kind == Some(RowKind::ToolResult)
        && let Some(id) = rows[focus_idx].entry_id
    {
        return entry_run(rows, id);
    }
    // Otherwise the nearest tool-result row at or above focus.
    let idx = rows[..=focus_idx]
        .iter()
        .rposition(|r| r.entry_kind == Some(RowKind::ToolResult))?;
    let id = rows[idx].entry_id?;
    entry_run(rows, id)
}

/// The interior rows of the fenced code block bracketing `focus_idx` (the
/// fence lines themselves are excluded). Walks the rows toggling an in-fence
/// flag, finding the `[open, close]` fence pair that contains the focus. Tests
/// the *gutter-stripped* text so a rail-prefixed fence still registers. Returns
/// `None` when the focus is not inside any closed fence pair.
fn resolve_code_block(rows: &[TranscriptRow], focus_idx: usize) -> Option<(usize, usize)> {
    let is_fence = |idx: usize| crate::streaming::line_is_fence(strip_gutter(&rows[idx].copy_text));

    let mut open: Option<usize> = None;
    for idx in 0..rows.len() {
        if !is_fence(idx) {
            continue;
        }
        match open {
            None => open = Some(idx),
            Some(start) => {
                // `start`..=`idx` is a complete fence pair. If focus is inside
                // it (inclusive of the fence lines themselves), return the
                // interior. Continue scanning otherwise.
                if (start..=idx).contains(&focus_idx) {
                    if idx > start + 1 {
                        return Some((start + 1, idx - 1));
                    }
                    // Empty fenced block (``` immediately followed by ```).
                    return None;
                }
                open = None;
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Render the resolved `rows[lo..=hi]` in `format`. `is_assistant` is consulted
/// by the Markdown heading and ignored by Plain/JsonSlice's kind tags.
pub(crate) fn format_rows(
    rows: &[TranscriptRow],
    lo: usize,
    hi: usize,
    format: CopyFormat,
    is_assistant: &dyn Fn(EntryId) -> bool,
) -> String {
    let slice = &rows[lo..=hi.min(rows.len().saturating_sub(1))];
    match format {
        CopyFormat::Plain => format_plain(slice),
        CopyFormat::Markdown => format_markdown(slice, is_assistant),
        CopyFormat::JsonSlice => format_json_slice(slice),
    }
}

/// Plain text: each row's gutter-stripped `copy_text`, one per line. Trailing
/// blank lines (the entry's wrapped block often ends on a spacer row) are
/// dropped so a pasted answer doesn't carry trailing whitespace.
fn format_plain(rows: &[TranscriptRow]) -> String {
    let joined = rows
        .iter()
        .map(|r| strip_gutter(&r.copy_text))
        .collect::<Vec<_>>()
        .join("\n");
    joined.trim_end().to_string()
}

/// Markdown: gutter-stripped text with each message entry prefixed by a
/// `**Assistant**` / `**User**` heading derived from its kind + role, operating
/// on entry granularity using `entry_id` runs. Any ``` fence rows already
/// present in the transcript are emitted verbatim; while a fence is open the
/// heading emission is suppressed so a `"```"` boundary is never mistaken for an
/// entry change. The formatter does not synthesize or re-fence code blocks —
/// the fences it preserves are the ones the rows already carried.
fn format_markdown(rows: &[TranscriptRow], is_assistant: &dyn Fn(EntryId) -> bool) -> String {
    let mut out = String::new();
    let mut prev_entry: Option<Option<EntryId>> = None;
    let mut in_fence = false;

    for row in rows {
        let text = strip_gutter(&row.copy_text);

        // Emit a heading when crossing into a new message entry (not while we
        // are inside a fenced block, where a "```" boundary must not be
        // mistaken for an entry change).
        if !in_fence
            && prev_entry != Some(row.entry_id)
            && row.entry_kind == Some(RowKind::Message)
            && let Some(id) = row.entry_id
        {
            if !out.is_empty() {
                out.push('\n');
            }
            let heading = if is_assistant(id) {
                "**Assistant**"
            } else {
                "**User**"
            };
            out.push_str(heading);
            out.push('\n');
        }
        prev_entry = Some(row.entry_id);

        if crate::streaming::line_is_fence(text) {
            in_fence = !in_fence;
        }
        out.push_str(text);
        out.push('\n');
    }
    // Trim the trailing newline so callers get a tidy block.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// JSON event slice: a JSON array of `{ "id", "kind", "text" }` objects, one
/// per resolved *entry* (rows grouped by `entry_id`). Chrome rows (no
/// `entry_id`) are skipped. `text` is the gutter-stripped plain text of the
/// entry's rows joined by newlines.
fn format_json_slice(rows: &[TranscriptRow]) -> String {
    #[derive(serde::Serialize)]
    struct Event {
        id: u64,
        kind: &'static str,
        text: String,
    }

    let mut events: Vec<Event> = Vec::new();
    let mut idx = 0;
    while idx < rows.len() {
        let Some(entry_id) = rows[idx].entry_id else {
            idx += 1;
            continue;
        };
        // Gather the contiguous run for this entry id.
        let mut lines: Vec<&str> = Vec::new();
        let kind = rows[idx].entry_kind.map(|k| k.slug()).unwrap_or("message");
        while idx < rows.len() && rows[idx].entry_id == Some(entry_id) {
            lines.push(strip_gutter(&rows[idx].copy_text));
            idx += 1;
        }
        events.push(Event {
            id: entry_id.0,
            kind,
            text: lines.join("\n"),
        });
    }

    // Serialize with the crate's serde_json dependency. A serialization failure
    // is impossible for this shape, but degrade to `[]` rather than panic.
    serde_json::to_string_pretty(&events).unwrap_or_else(|_| "[]".to_string())
}

/// One-shot helper: resolve `scope` and format it, returning the payload string
/// (or `None` when the scope resolves to nothing). This is the single entry
/// `lib.rs` calls after building the rows.
#[allow(clippy::too_many_arguments)]
pub(crate) fn gather(
    rows: &[TranscriptRow],
    focus: Option<RowId>,
    scope: CopyScope,
    format: CopyFormat,
    is_assistant: &dyn Fn(EntryId) -> bool,
    viewport: Option<(RowId, RowId)>,
) -> Option<String> {
    let (lo, hi) = resolve_scope(rows, focus, scope, is_assistant, viewport)?;
    Some(format_rows(rows, lo, hi, format, is_assistant))
}

#[cfg(test)]
#[path = "copy_tests.rs"]
mod tests;
