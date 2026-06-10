//! Scratchpad Pane (§12.3.3).
//!
//! A session-scoped notes/composition pane for observations, quotes, draft
//! prompts, and temporary task structure. It is a side pane the user opens over
//! the transcript: the live transcript stays on the LEFT (context preserved)
//! while an editable scratchpad on the RIGHT collects free text. The buffer
//! survives across turns — it lives in session UI state, not the model
//! transcript — and **never enters model context** unless the user explicitly
//! inserts it into the composer or queues it as a prompt.
//!
//! ## Model, not chrome
//!
//! Like its peer leaf modules ([`crate::snippet_store`],
//! [`crate::clipboard_history`]) this file owns only the *pure* editable model
//! plus its source-link provenance, so every cursor/edit/clear rule is
//! unit-testable without standing up a `TuiApp` or a terminal. `lib.rs` owns the
//! side effects: the keybinding, the open/close flag, the per-frame render call
//! through the single fullscreen `render()`, the send-to-composer / queue
//! verbs, and the status line. The crate root reuses the §11G.10 detail-pane
//! split machinery ([`crate::diff_detail_pane::split_overlay_content`]) to carve
//! the side pane, falling back to a centered overlay when the terminal is too
//! narrow to split.
//!
//! It owns:
//!
//!   - [`SourceLink`]: a provenance breadcrumb the user inserted — where a quote
//!     or note came from (a transcript entry id plus a short label). The spec
//!     calls for retaining source links even though the visible note stays
//!     concise.
//!   - [`Scratchpad`]: the editable text buffer, a byte-offset cursor, a dirty
//!     flag, and the bounded list of source links. Editing primitives mirror the
//!     composer's (insert char/text, delete back/forward, cursor moves, newline)
//!     so the pane edits exactly like the composer the spec says to reuse.
//!
//! ## Bounds
//!
//! [`MAX_LINKS`] caps the retained source-link breadcrumbs (oldest dropped
//! first) so a long session never grows the provenance list without bound. The
//! text buffer itself is unbounded by design — it is the user's own scratch
//! space — but the pane only ever paints a windowed view, so a large buffer
//! never bloats a frame.

/// Largest number of source-link breadcrumbs retained. Small on purpose: links
/// are a "remember where this came from" affordance, not a database. Appending
/// past this drops the oldest so the list stays bounded.
pub(crate) const MAX_LINKS: usize = 32;

/// Largest number of characters retained in a source-link's derived label. The
/// label is a one-line handle; the link's full provenance is the entry id it
/// carries.
pub(crate) const LABEL_CHARS: usize = 48;

/// A provenance breadcrumb appended to the scratchpad: where an inserted note or
/// quote came from. Keyed by the STABLE transcript entry id (never a Vec index),
/// so a streamed/coalesced transcript mutation never repoints a link at the
/// wrong entry. The `label` is a concise human handle derived at insert time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceLink {
    /// `TranscriptEntry::id` the breadcrumb points back at.
    pub(crate) entry_id: u64,
    /// A concise one-line handle shown beside the breadcrumb.
    pub(crate) label: String,
}

/// The editable scratchpad model: a free-text buffer, a byte-offset cursor, a
/// dirty flag, and the bounded list of source links.
///
/// The buffer is plain UTF-8 with embedded `\n` for line breaks, exactly like
/// the composer's `input`; the cursor is a byte offset always kept on a char
/// boundary by the editing primitives. `dirty` flips on the first edit and is
/// cleared by [`mark_clean`](Self::mark_clean) (set after a send/queue/export so
/// the pane can show "saved" provenance without re-flagging unchanged text).
#[derive(Debug, Clone, Default)]
pub(crate) struct Scratchpad {
    /// The note buffer. Plain UTF-8 with embedded newlines.
    text: String,
    /// Byte offset of the edit caret into `text`. Always on a char boundary.
    cursor: usize,
    /// Whether the buffer has unsaved edits since the last [`mark_clean`].
    dirty: bool,
    /// Bounded, newest-last list of source-link breadcrumbs.
    links: Vec<SourceLink>,
}

impl Scratchpad {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The full buffer text, handed verbatim to the composer / queue / export.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Whether the buffer has unsaved edits since the last [`mark_clean`].
    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Whether the buffer has no characters (an empty scratchpad shows its
    /// empty-state hint).
    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Read-only view of the source-link breadcrumbs, oldest first.
    pub(crate) fn links(&self) -> &[SourceLink] {
        &self.links
    }

    /// Number of characters in the buffer (NOT bytes), for the status line.
    pub(crate) fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    /// Number of lines the buffer holds (1-based: an empty buffer is one line, a
    /// buffer ending in `\n` counts the trailing empty line).
    pub(crate) fn line_count(&self) -> usize {
        self.text.bytes().filter(|&b| b == b'\n').count() + 1
    }

    /// Clear the dirty flag. Called after a send/queue/export so the pane stops
    /// flagging text that has been delivered somewhere.
    pub(crate) fn mark_clean(&mut self) {
        self.dirty = false;
    }

    /// Insert a single character at the caret, advancing the caret past it and
    /// flagging the buffer dirty. The caret is re-clamped onto a char boundary
    /// first so a desynced offset never splits a multi-byte char.
    pub(crate) fn insert_char(&mut self, ch: char) {
        self.clamp_cursor();
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.dirty = true;
    }

    /// Insert a string at the caret, advancing the caret past it and flagging the
    /// buffer dirty. A no-op on empty text (so a paste of nothing never flags
    /// dirty). The caret is re-clamped first.
    pub(crate) fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.clamp_cursor();
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.dirty = true;
    }

    /// Append `text` to the END of the buffer on its own line (the "append quote"
    /// verb), separating from any existing content with a newline, then park the
    /// caret at the end. Flags dirty. A no-op on empty `text`.
    ///
    /// Used by the crate root's "append the active selection / a quote" path so a
    /// quote always lands as a fresh block rather than splicing into mid-line text
    /// wherever the caret happened to be.
    pub(crate) fn append_block(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
        }
        self.text.push_str(text);
        self.cursor = self.text.len();
        self.dirty = true;
    }

    /// Append a source-link breadcrumb (the "insert source link" verb): record
    /// the provenance AND splice a concise reference line into the buffer at the
    /// end. The label is flattened + clipped to [`LABEL_CHARS`]. Enforces
    /// [`MAX_LINKS`] by dropping the oldest breadcrumb. Flags dirty.
    pub(crate) fn append_source_link(&mut self, entry_id: u64, label: &str) {
        let label = flatten_one_line(label, LABEL_CHARS);
        // The visible reference line is a concise, copy-safe handle; the entry id
        // lives in the retained breadcrumb for a future "jump to source".
        let reference = if label.is_empty() {
            format!("[source: entry #{entry_id}]")
        } else {
            format!("[source: {label} (entry #{entry_id})]")
        };
        self.append_block(&reference);
        self.links.push(SourceLink {
            entry_id,
            label: if label.is_empty() {
                format!("entry #{entry_id}")
            } else {
                label
            },
        });
        while self.links.len() > MAX_LINKS {
            self.links.remove(0);
        }
    }

    /// Delete the char immediately before the caret (Backspace), moving the caret
    /// back onto its boundary and flagging dirty. A no-op at the start of the
    /// buffer.
    pub(crate) fn delete_back(&mut self) {
        self.clamp_cursor();
        if self.cursor == 0 {
            return;
        }
        let prev = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        self.dirty = true;
    }

    /// Delete the char at the caret (Delete/forward), leaving the caret put and
    /// flagging dirty. A no-op at the end of the buffer.
    pub(crate) fn delete_forward(&mut self) {
        self.clamp_cursor();
        if self.cursor >= self.text.len() {
            return;
        }
        let next = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or(self.text.len());
        self.text.replace_range(self.cursor..next, "");
        self.dirty = true;
    }

    /// Move the caret one char left. Saturates at the start of the buffer.
    pub(crate) fn move_left(&mut self) {
        self.clamp_cursor();
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }

    /// Move the caret one char right. Saturates at the end of the buffer.
    pub(crate) fn move_right(&mut self) {
        self.clamp_cursor();
        if self.cursor >= self.text.len() {
            return;
        }
        self.cursor = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or(self.text.len());
    }

    /// Move the caret to the start of the buffer.
    pub(crate) fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Move the caret to the end of the buffer.
    pub(crate) fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    /// Clear the entire buffer, source links, and reset the caret (the "clear"
    /// verb). Flags dirty only when there was content to clear, so clearing an
    /// already-empty pad never spuriously dirties it.
    pub(crate) fn clear(&mut self) {
        if self.text.is_empty() && self.links.is_empty() {
            return;
        }
        self.text.clear();
        self.links.clear();
        self.cursor = 0;
        self.dirty = true;
    }

    /// The buffer split into display lines (by `\n`). Always returns at least one
    /// (possibly empty) line, so the renderer never has to special-case an empty
    /// buffer.
    pub(crate) fn lines(&self) -> Vec<&str> {
        if self.text.is_empty() {
            return vec![""];
        }
        // `split('\n')` keeps a trailing empty segment after a final newline, so a
        // buffer ending in `\n` shows the empty line the caret sits on.
        self.text.split('\n').collect()
    }

    /// The caret's `(line, column)` in CHAR units, for placing the render cursor.
    /// `line` is the 0-based line the caret is on; `column` is the char offset
    /// into that line. Walks the buffer up to the byte cursor once.
    pub(crate) fn cursor_line_col(&self) -> (usize, usize) {
        let upto = &self.text[..self.cursor.min(self.text.len())];
        let line = upto.bytes().filter(|&b| b == b'\n').count();
        let col = match upto.rfind('\n') {
            Some(nl) => upto[nl + 1..].chars().count(),
            None => upto.chars().count(),
        };
        (line, col)
    }

    /// Re-clamp the caret onto the nearest char boundary at or below its current
    /// value, and never past the buffer end. Cheap when already valid. Guards
    /// every editing primitive against a desynced offset.
    fn clamp_cursor(&mut self) {
        if self.cursor > self.text.len() {
            self.cursor = self.text.len();
        }
        while self.cursor > 0 && !self.text.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
    }
}

/// Flatten `text` to a single line — interior `\n`/`\r`/`\t` runs collapse to one
/// space — then clip to `max_chars` with a trailing `…` when over. Mirrors the
/// snippet store's name derivation so the two never drift in their flattening.
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
    clipped.push('\u{2026}');
    clipped
}

#[cfg(test)]
#[path = "scratchpad_tests.rs"]
mod tests;
