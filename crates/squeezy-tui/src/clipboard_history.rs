//! In-app clipboard history (§12.6.1).
//!
//! Every copy Squeezy initiates already funnels through one service
//! ([`crate::deliver_copy`]); this module is the bounded memory of *those*
//! payloads, so a user can recover, re-copy, or delete a recent copy from inside
//! the app without Squeezy ever reading the arbitrary OS clipboard. The privacy
//! contract is one-directional by construction: nothing here scrapes the system
//! clipboard — entries are only ever *recorded* by the copy service, never read
//! back from the host.
//!
//! ## Model, not chrome
//!
//! This module is deliberately pure. It owns:
//!
//!   - [`ClipboardEntry`]: one recorded copy — a stable id, the full payload, a
//!     short human label (the copy scope, e.g. "assistant message"), the byte
//!     length, and a `pinned` flag.
//!   - [`ClipboardHistoryStore`]: a bounded ring buffer of entries (newest
//!     first) with max-entry and max-byte caps, pinned retention across
//!     eviction, deletion, clear, and a selection cursor for the picker overlay.
//!
//! `lib.rs` owns the side effects: it calls [`ClipboardHistoryStore::record`]
//! from the single copy service, opens/closes the picker, paints it through the
//! one fullscreen `render()`, and re-delivers a chosen entry through the same
//! clipboard provider chain. Keeping the bookkeeping here makes every cap,
//! eviction, and cursor-movement rule unit-testable without standing up a
//! `TuiApp` or a terminal.
//!
//! ## Bounds
//!
//! Two caps keep the store's footprint bounded so an idle session never grows
//! without limit:
//!
//!   - [`MAX_ENTRIES`] caps the entry *count*. Recording past it evicts the
//!     oldest **unpinned** entry.
//!   - [`MAX_TOTAL_BYTES`] caps the summed payload *bytes*. Recording evicts
//!     oldest-unpinned entries until the new total fits (or only pinned entries
//!     remain). A single payload larger than the whole cap is still recorded
//!     (truncating it would corrupt a re-copy); it just forces every unpinned
//!     entry out.
//!
//! Pinned entries are never evicted by either cap, matching the spec's "pinned
//! retention" rule — an explicitly pinned copy survives until the user unpins or
//! deletes it.

/// Largest number of entries retained. Small on purpose: the history is a
/// "recover my last few copies" affordance, not a clipboard manager database. A
/// deep list would make the picker unwieldy and the eviction unpredictable.
/// Recording past this evicts the oldest unpinned entry.
pub(crate) const MAX_ENTRIES: usize = 32;

/// Largest summed payload size (bytes) retained across all entries. Bounds the
/// store's memory so a session that copies many large transcripts does not grow
/// without limit; recording evicts oldest-unpinned entries until the new total
/// fits. 256 KiB comfortably holds dozens of normal copies while capping a
/// pathological full-transcript-spam session.
pub(crate) const MAX_TOTAL_BYTES: usize = 256 * 1024;

/// Largest number of body characters the picker shows for one entry's preview.
/// A copied payload can be thousands of characters; the picker only ever shows a
/// bounded single-line snippet, so the overlay stays a fixed size regardless of
/// payload length.
pub(crate) const PREVIEW_CHARS: usize = 80;

/// One recorded copy.
///
/// Holds the full `text` (so a re-copy reproduces the original payload exactly),
/// plus the metadata the picker shows: a stable monotonic `id`, the copy
/// `label` (the scope the copy targeted, e.g. `"assistant message"`), the
/// payload `bytes`, and whether the entry is `pinned` (exempt from eviction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClipboardEntry {
    /// Stable, monotonically-increasing id. Never reused within a session, so a
    /// picker selection / delete keyed by id never lands on the wrong entry
    /// after an eviction shifts the list.
    pub(crate) id: u64,
    /// The full copied payload, handed back verbatim on re-copy.
    pub(crate) text: String,
    /// Short human label for the copy's scope (the `CopyScope::label`), shown in
    /// the picker as the entry's heading.
    pub(crate) label: String,
    /// UTF-8 byte length of `text`, precomputed so the picker stat line and the
    /// byte-cap math never re-walk the string.
    pub(crate) bytes: usize,
    /// Whether this entry is pinned (never evicted by the entry/byte caps).
    pub(crate) pinned: bool,
}

impl ClipboardEntry {
    /// A bounded single-line preview of the payload for the picker row: the
    /// first [`PREVIEW_CHARS`] characters with interior newlines/tabs flattened
    /// to a single space and a trailing `…` when clipped. Pure presentation; the
    /// full `text` is untouched.
    pub(crate) fn preview(&self) -> String {
        // Flatten control whitespace so a multi-line copy shows as one tidy row.
        let mut flattened = String::with_capacity(self.text.len().min(PREVIEW_CHARS * 2));
        let mut last_was_space = false;
        for ch in self.text.chars() {
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
        if count <= PREVIEW_CHARS {
            return flattened.to_string();
        }
        let keep = PREVIEW_CHARS.saturating_sub(1);
        let mut clipped: String = flattened.chars().take(keep).collect();
        clipped.push('…');
        clipped
    }
}

/// Bounded, newest-first ring of recorded copies plus the picker's selection
/// cursor.
///
/// Entries are stored with index 0 = newest. The caps ([`MAX_ENTRIES`],
/// [`MAX_TOTAL_BYTES`]) are enforced on every [`record`](Self::record) by
/// evicting oldest **unpinned** entries; pinned entries are exempt. The
/// `selected` cursor is the picker's highlighted row, kept in range as the list
/// shrinks/grows.
#[derive(Debug, Clone, Default)]
pub(crate) struct ClipboardHistoryStore {
    entries: Vec<ClipboardEntry>,
    /// Monotonic id source. Never reset within a session.
    next_id: u64,
    /// The picker's highlighted index (into `entries`, newest-first). Clamped to
    /// a valid row whenever the list changes; meaningless when empty.
    selected: usize,
}

impl ClipboardHistoryStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record a copy of `text` labelled `label`, returning the new entry's id.
    ///
    /// Newest-first: the entry is inserted at the front. A back-to-back
    /// **duplicate** of the current newest entry (same text *and* same label) is
    /// collapsed — it is not re-inserted; instead the existing front entry's id
    /// is returned — so hammering the same copy chord does not bury the list with
    /// identical rows. Enforces both caps afterwards by evicting oldest-unpinned
    /// entries. The selection cursor follows the freshly-recorded (or matched)
    /// entry to the front.
    pub(crate) fn record(&mut self, text: &str, label: &str) -> u64 {
        if let Some(front) = self.entries.first()
            && front.text == text
            && front.label == label
        {
            // Exact repeat of the newest copy: no new row, just re-point the
            // cursor at it.
            self.selected = 0;
            return front.id;
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let entry = ClipboardEntry {
            id,
            bytes: text.len(),
            text: text.to_string(),
            label: label.to_string(),
            pinned: false,
        };
        self.entries.insert(0, entry);
        self.selected = 0;
        self.enforce_caps();
        id
    }

    /// Evict oldest-unpinned entries until both caps are satisfied. Pinned
    /// entries are never evicted; if only pinned entries remain, the caps may be
    /// (legitimately) exceeded and eviction stops rather than dropping a pin.
    fn enforce_caps(&mut self) {
        // Entry-count cap: drop the oldest unpinned while over.
        while self.entries.len() > MAX_ENTRIES {
            if !self.evict_oldest_unpinned() {
                break;
            }
        }
        // Byte cap: drop oldest unpinned while the summed payload is over, but
        // never evict the last surviving entry — a single payload larger than the
        // whole cap is still kept (truncating it would corrupt a re-copy); it just
        // forces every *other* unpinned entry out.
        while self.entries.len() > 1 && self.total_bytes() > MAX_TOTAL_BYTES {
            if !self.evict_oldest_unpinned() {
                break;
            }
        }
        self.clamp_selection();
    }

    /// Remove the oldest (highest-index) unpinned entry. Returns `true` when one
    /// was removed, `false` when every remaining entry is pinned.
    fn evict_oldest_unpinned(&mut self) -> bool {
        if let Some(pos) = self.entries.iter().rposition(|e| !e.pinned) {
            self.entries.remove(pos);
            true
        } else {
            false
        }
    }

    /// Summed payload bytes across all entries.
    pub(crate) fn total_bytes(&self) -> usize {
        self.entries.iter().map(|e| e.bytes).sum()
    }

    /// Number of entries currently held.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the history is empty (the picker shows an empty-state line).
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Read-only view of the entries, newest first.
    pub(crate) fn entries(&self) -> &[ClipboardEntry] {
        &self.entries
    }

    /// The picker's currently-selected index (newest-first). Meaningless when
    /// empty; callers gate on [`is_empty`](Self::is_empty) first.
    pub(crate) fn selected_index(&self) -> usize {
        self.selected
    }

    /// The currently-selected entry, or `None` when the history is empty.
    pub(crate) fn selected_entry(&self) -> Option<&ClipboardEntry> {
        self.entries.get(self.selected)
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
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }

    /// Point the cursor at the entry with `id`, returning `true` when it exists.
    /// Used by the mouse path: a click resolves a row to its stable id, then
    /// selects it — so a concurrent eviction can never select the wrong row.
    pub(crate) fn select_id(&mut self, id: u64) -> bool {
        if let Some(pos) = self.entries.iter().position(|e| e.id == id) {
            self.selected = pos;
            true
        } else {
            false
        }
    }

    /// The full payload of the entry with `id`, for re-delivery to the clipboard.
    /// `None` when no such entry exists (it was evicted/deleted meanwhile).
    pub(crate) fn text_of(&self, id: u64) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.text.as_str())
    }

    /// Toggle the pinned flag of the entry with `id`. Returns the entry's new
    /// pinned state, or `None` when no such entry exists. A pinned entry is
    /// exempt from eviction until unpinned or deleted.
    pub(crate) fn toggle_pin(&mut self, id: u64) -> Option<bool> {
        let entry = self.entries.iter_mut().find(|e| e.id == id)?;
        entry.pinned = !entry.pinned;
        Some(entry.pinned)
    }

    /// Delete the entry with `id`, returning `true` when one was removed. Keeps
    /// the selection cursor on a valid row (it stays put, then clamps, so the
    /// row that slid up into the deleted slot becomes selected).
    pub(crate) fn delete(&mut self, id: u64) -> bool {
        if let Some(pos) = self.entries.iter().position(|e| e.id == id) {
            self.entries.remove(pos);
            self.clamp_selection();
            true
        } else {
            false
        }
    }

    /// Drop every entry — pinned included — and reset the cursor. The visible
    /// "clear" verb the spec requires; deliberately unconditional so a privacy-
    /// minded user can wipe the in-app history in one action.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.selected = 0;
    }

    /// Keep `selected` within `[0, len)`; clamp to the last row when the list
    /// shrank past it, and to 0 when it emptied.
    fn clamp_selection(&mut self) {
        if self.entries.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.entries.len() {
            self.selected = self.entries.len() - 1;
        }
    }
}

#[cfg(test)]
#[path = "clipboard_history_tests.rs"]
mod tests;
