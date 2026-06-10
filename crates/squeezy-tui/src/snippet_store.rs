//! Prompt Snippets From Selection (§12.3.2).
//!
//! Turns a transcript SELECTION into a reusable, named prompt snippet that the
//! user can drop back into the composer or stage onto the prompt queue. The
//! capture path reuses the same gutter-stripped clean text a copy/quote uses
//! ([`crate::selection::selection_clean_text`]); this module owns only the
//! *pure* model — the snippet record, its provenance, and the bounded store the
//! picker overlay browses.
//!
//! ## Model, not chrome
//!
//! Like [`crate::clipboard_history`], this module is deliberately side-effect
//! free so every cap, cursor, and name-derivation rule is unit-testable without
//! standing up a `TuiApp` or a terminal. `lib.rs` owns the side effects: reading
//! the live selection, calling [`crate::input::insert_input_text`] /
//! `prompt_queue.push_back`, opening/closing the picker, painting it through the
//! one fullscreen `render()`, and writing the status line.
//!
//! It owns:
//!
//!   - [`SnippetSource`]: where a snippet came from — the surface, the inclusive
//!     visual row range it spanned, and its char/byte size. Provenance the spec
//!     asks to retain internally even though the visible text stays concise.
//!   - [`Snippet`]: one saved snippet — a stable id, a concise human name, the
//!     full text, and its [`SnippetSource`].
//!   - [`SnippetStore`]: a bounded, newest-first ring of snippets with a picker
//!     selection cursor plus save / select / delete / clear.
//!
//! ## Bounds + size warning
//!
//! [`MAX_SNIPPETS`] caps the count (oldest dropped first). [`LARGE_SNIPPET_BYTES`]
//! is the threshold above which the picker flags a snippet as "large" — a
//! prompt-bloat warning the spec calls for ("Large snippets can bloat prompts;
//! preview and warn") — without ever rejecting it.

/// Largest number of snippets retained. Small on purpose: snippets are a
/// "stash a few reusable bits" affordance, not a database. Saving past this
/// drops the oldest snippet so the store stays bounded and the picker stays a
/// fixed, scannable size.
pub(crate) const MAX_SNIPPETS: usize = 32;

/// Largest number of characters retained in a snippet's derived NAME. The name
/// is a one-line handle the picker shows; the full text is untouched.
pub(crate) const NAME_CHARS: usize = 48;

/// Byte size at/above which a snippet is flagged "large" in the picker — a
/// prompt-bloat warning, never a hard reject (the spec wants preview-and-warn,
/// not truncation, so a re-insert reproduces the snippet exactly). 4 KiB is a
/// few hundred lines of prose/code: comfortably past any normal quote, well
/// short of a full transcript dump.
pub(crate) const LARGE_SNIPPET_BYTES: usize = 4 * 1024;

/// Largest number of body characters the picker shows for one snippet's preview.
/// A snippet can be thousands of characters; the picker only ever shows a
/// bounded single-line snippet, so the overlay stays a fixed size regardless of
/// payload length.
pub(crate) const PREVIEW_CHARS: usize = 72;

/// Which painted surface a snippet was captured from. Mirrors
/// [`crate::selection::SelectionSurface`] but is kept as its own small enum so
/// this module needs no dependency back on the selection model — the capture
/// site maps one onto the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnippetSurface {
    /// The always-on main transcript pane.
    Main,
    /// The Ctrl+T full-transcript overlay.
    Overlay,
}

/// Internal provenance of a snippet: where it came from and how big it was at
/// capture. Retained so a later feature (export, "jump to source", de-dup) can
/// reason about origin even though the picker only shows the concise name +
/// preview. The `row_start..=row_end` pair is the inclusive visual row span of
/// the originating selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SnippetSource {
    /// The surface the selection was painted on.
    pub(crate) surface: SnippetSurface,
    /// First visual row of the originating selection (inclusive).
    pub(crate) row_start: usize,
    /// Last visual row of the originating selection (inclusive).
    pub(crate) row_end: usize,
    /// Char count of the captured clean text.
    pub(crate) chars: usize,
    /// UTF-8 byte length of the captured clean text.
    pub(crate) bytes: usize,
}

impl SnippetSource {
    /// Number of visual rows the originating selection spanned (1-based: a
    /// single-row selection spans one row).
    pub(crate) fn row_count(&self) -> usize {
        self.row_end.saturating_sub(self.row_start) + 1
    }
}

/// One saved snippet.
///
/// Holds the full `text` (so a re-insert reproduces the original exactly), a
/// concise human `name` (derived from the first non-empty line by
/// [`derive_name`]), a stable monotonic `id`, and the [`SnippetSource`]
/// provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snippet {
    /// Stable, monotonically-increasing id. Never reused within a session, so a
    /// picker selection / delete keyed by id never lands on the wrong snippet
    /// after a drop shifts the list.
    pub(crate) id: u64,
    /// Concise one-line handle shown in the picker.
    pub(crate) name: String,
    /// The full snippet body, handed back verbatim on insert/enqueue.
    pub(crate) text: String,
    /// Where the snippet was captured from (internal provenance).
    pub(crate) source: SnippetSource,
}

impl Snippet {
    /// UTF-8 byte length of the body. Precomputed via the source so the picker
    /// stat line and the size-warning check never re-walk the string.
    pub(crate) fn bytes(&self) -> usize {
        self.source.bytes
    }

    /// Whether the snippet is "large" — at/above [`LARGE_SNIPPET_BYTES`]. The
    /// picker flags these so the user is warned before bloating a prompt.
    pub(crate) fn is_large(&self) -> bool {
        self.source.bytes >= LARGE_SNIPPET_BYTES
    }

    /// A bounded single-line preview of the body for the picker row: the first
    /// [`PREVIEW_CHARS`] characters with interior newlines/tabs flattened to a
    /// single space and a trailing `…` when clipped. Pure presentation; the full
    /// `text` is untouched.
    pub(crate) fn preview(&self) -> String {
        flatten_one_line(&self.text, PREVIEW_CHARS)
    }
}

/// Derive a concise snippet name from `text`: the first non-empty line,
/// whitespace-flattened and clipped to [`NAME_CHARS`] with a trailing `…`. Falls
/// back to `"(empty snippet)"` when `text` has no non-whitespace content (the
/// caller already gates on emptiness, so this is a defensive default).
pub(crate) fn derive_name(text: &str) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if first.is_empty() {
        return "(empty snippet)".to_string();
    }
    flatten_one_line(first, NAME_CHARS)
}

/// Flatten `text` to a single line — interior `\n`/`\r`/`\t` runs collapse to one
/// space — then clip to `max_chars` with a trailing `…` when over. Shared by the
/// name derivation and the preview so the two never drift in their flattening.
fn flatten_one_line(text: &str, max_chars: usize) -> String {
    let mut flattened = String::with_capacity(text.len().min(max_chars * 2));
    let mut last_was_space = false;
    for ch in text.chars() {
        let c = if ch == '\n' || ch == '\r' || ch == '\t' {
            ' '
        } else {
            ch
        };
        if c == ' ' {
            if last_was_space {
                continue;
            }
            last_was_space = true;
        } else {
            last_was_space = false;
        }
        flattened.push(c);
    }
    let flattened = flattened.trim();
    let count = flattened.chars().count();
    if count <= max_chars {
        return flattened.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut clipped: String = flattened.chars().take(keep).collect();
    clipped.push('…');
    clipped
}

/// Bounded, newest-first ring of saved snippets plus the picker's selection
/// cursor.
///
/// Entries are stored with index 0 = newest. [`MAX_SNIPPETS`] is enforced on
/// every [`save`](Self::save) by dropping the oldest. The `selected` cursor is
/// the picker's highlighted row, kept in range as the list shrinks/grows.
#[derive(Debug, Clone, Default)]
pub(crate) struct SnippetStore {
    snippets: Vec<Snippet>,
    /// Monotonic id source. Never reset within a session.
    next_id: u64,
    /// The picker's highlighted index (into `snippets`, newest-first). Clamped to
    /// a valid row whenever the list changes; meaningless when empty.
    selected: usize,
}

impl SnippetStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Save `text` with provenance `source`, returning the new snippet's id (or
    /// `None` when `text` has no non-whitespace content — there is nothing to
    /// save). The name is derived from the first non-empty line. Newest-first:
    /// the snippet is inserted at the front and the cursor follows it there.
    /// Enforces [`MAX_SNIPPETS`] afterwards by dropping the oldest.
    pub(crate) fn save(&mut self, text: &str, source: SnippetSource) -> Option<u64> {
        if text.trim().is_empty() {
            return None;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let snippet = Snippet {
            id,
            name: derive_name(text),
            text: text.to_string(),
            source,
        };
        self.snippets.insert(0, snippet);
        self.selected = 0;
        self.enforce_cap();
        Some(id)
    }

    /// Drop oldest snippets until at most [`MAX_SNIPPETS`] remain, then re-clamp
    /// the cursor.
    fn enforce_cap(&mut self) {
        while self.snippets.len() > MAX_SNIPPETS {
            self.snippets.pop();
        }
        self.clamp_selection();
    }

    /// Number of snippets currently held.
    pub(crate) fn len(&self) -> usize {
        self.snippets.len()
    }

    /// Whether the store is empty (the picker shows an empty-state line).
    pub(crate) fn is_empty(&self) -> bool {
        self.snippets.is_empty()
    }

    /// Read-only view of the snippets, newest first.
    pub(crate) fn snippets(&self) -> &[Snippet] {
        &self.snippets
    }

    /// The picker's currently-selected index (newest-first). Meaningless when
    /// empty; callers gate on [`is_empty`](Self::is_empty) first.
    pub(crate) fn selected_index(&self) -> usize {
        self.selected
    }

    /// The currently-selected snippet, or `None` when the store is empty.
    pub(crate) fn selected_snippet(&self) -> Option<&Snippet> {
        self.snippets.get(self.selected)
    }

    /// Move the picker cursor up one row (toward the newest). Saturates at the
    /// top; a no-op on an empty list.
    pub(crate) fn select_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Move the picker cursor down one row (toward the oldest). Saturates at the
    /// last row; a no-op on an empty list.
    pub(crate) fn select_down(&mut self) {
        if self.selected + 1 < self.snippets.len() {
            self.selected += 1;
        }
    }

    /// Point the cursor at the snippet with `id`, returning `true` when it
    /// exists. Used by the mouse path: a click resolves a row to its stable id,
    /// then selects it — so a concurrent drop can never select the wrong row.
    pub(crate) fn select_id(&mut self, id: u64) -> bool {
        if let Some(pos) = self.snippets.iter().position(|s| s.id == id) {
            self.selected = pos;
            true
        } else {
            false
        }
    }

    /// The full body of the snippet with `id`, for re-insertion. `None` when no
    /// such snippet exists (it was dropped/deleted meanwhile).
    pub(crate) fn text_of(&self, id: u64) -> Option<&str> {
        self.snippets
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.text.as_str())
    }

    /// Delete the snippet with `id`, returning `true` when one was removed. Keeps
    /// the selection cursor on a valid row (it stays put, then clamps, so the row
    /// that slid up into the deleted slot becomes selected).
    pub(crate) fn delete(&mut self, id: u64) -> bool {
        if let Some(pos) = self.snippets.iter().position(|s| s.id == id) {
            self.snippets.remove(pos);
            self.clamp_selection();
            true
        } else {
            false
        }
    }

    /// Drop every snippet and reset the cursor.
    pub(crate) fn clear(&mut self) {
        self.snippets.clear();
        self.selected = 0;
    }

    /// Keep `selected` within `[0, len)`; clamp to the last row when the list
    /// shrank past it, and to 0 when it emptied.
    fn clamp_selection(&mut self) {
        if self.snippets.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.snippets.len() {
            self.selected = self.snippets.len() - 1;
        }
    }
}

#[cfg(test)]
#[path = "snippet_store_tests.rs"]
mod tests;
