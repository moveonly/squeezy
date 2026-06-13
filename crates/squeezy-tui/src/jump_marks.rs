//! Jump marks (§11.2 / backlog 11G.2): set a mark at the current transcript
//! row, jump back to the most recent mark, and keep a short recent-jump
//! history.
//!
//! Marks are stored as **stable entry ids** (`TranscriptEntry::id`), never as
//! raw row offsets. A row offset is a derived, width- and fold-dependent
//! coordinate that goes stale the instant the transcript reflows (resize,
//! streaming, collapse); the entry id is the one identity that survives all of
//! those. The jump path resolves an id back to a live row at jump time, so a
//! mark set before a reflow still lands on the right entry afterwards (and a
//! mark whose entry has since been dropped is simply skipped).
//!
//! This module is deliberately pure: it owns the mark/history bookkeeping and
//! nothing about geometry, rendering, or input. `lib.rs` captures the current
//! top-visible entry id, feeds it in, and converts the id the stack hands back
//! into a scroll target. That keeps the navigation math testable without a
//! terminal.

use std::collections::VecDeque;

/// Largest number of marks kept on the stack. Small on purpose: jump marks are
/// a lightweight "remember where I was" affordance, not a bookmark database. A
/// deep stack would make "jump back" unpredictable. Oldest marks fall off the
/// front when the cap is exceeded.
pub(crate) const MARK_STACK_CAP: usize = 16;

/// Largest number of recent jump destinations retained for the history readout.
/// Surfaced (newest first) so the user can see the trail of where jumps landed
/// without it growing without bound.
pub(crate) const HISTORY_CAP: usize = 16;

/// Pure jump-mark bookkeeping over stable entry ids.
///
/// Two collections, both keyed by `TranscriptEntry::id`:
///   - `marks`: a LIFO stack of explicitly-set marks. [`set`](Self::set) pushes
///     the current row's entry id; [`jump_back`](Self::jump_back) pops the most
///     recent and returns it. Consecutive duplicate sets collapse so hammering
///     "set mark" on one row doesn't bury the stack.
///   - `history`: a bounded, newest-first ring of jump destinations, fed every
///     time a jump-back actually lands. This is the "recent jump history" the
///     spec asks to expose; it is read-only to callers.
#[derive(Debug, Clone, Default)]
pub(crate) struct JumpMarkStack {
    marks: Vec<u64>,
    history: VecDeque<u64>,
}

impl JumpMarkStack {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record a mark at the entry currently at the top of the viewport.
    ///
    /// Returns the new mark count. A set that repeats the id already on top of
    /// the stack is a no-op (the user re-marked the same row) so the stack
    /// stays a meaningful trail of distinct positions. Exceeding
    /// [`MARK_STACK_CAP`] drops the oldest mark from the front.
    pub(crate) fn set(&mut self, entry_id: u64) -> usize {
        if self.marks.last() != Some(&entry_id) {
            self.marks.push(entry_id);
            while self.marks.len() > MARK_STACK_CAP {
                self.marks.remove(0);
            }
        }
        self.marks.len()
    }

    /// Pop the most recent mark and return the entry id to jump to, or `None`
    /// when there are no marks left. `current_id` is the entry id at the top of
    /// the viewport *before* the jump; it is recorded in the history trail so
    /// the readout shows where each jump came from, and a mark that just points
    /// back at the current row is skipped (jumping nowhere is not useful) —
    /// the next-older mark is used instead.
    pub(crate) fn jump_back(&mut self, current_id: Option<u64>) -> Option<u64> {
        while let Some(target) = self.marks.pop() {
            if Some(target) == current_id {
                // Mark points at where we already are; skip it.
                continue;
            }
            self.push_history(target);
            return Some(target);
        }
        None
    }

    fn push_history(&mut self, entry_id: u64) {
        // Newest first; collapse an immediate repeat so re-landing on the same
        // row twice in a row doesn't double the readout.
        if self.history.front() == Some(&entry_id) {
            return;
        }
        self.history.push_front(entry_id);
        while self.history.len() > HISTORY_CAP {
            self.history.pop_back();
        }
    }

    /// Number of marks currently on the stack.
    pub(crate) fn mark_count(&self) -> usize {
        self.marks.len()
    }

    /// The recent jump-destination history, newest first, capped at
    /// [`HISTORY_CAP`]. Test-only raw view; production reads it through
    /// [`history_summary`](Self::history_summary).
    #[cfg(test)]
    pub(crate) fn history(&self) -> &VecDeque<u64> {
        &self.history
    }

    /// A short one-line summary of the recent jump history for the status line
    /// (e.g. `"jumps: #4 ← #2 ← #1"`), newest first, at most `max` entries.
    /// `label` maps an entry id to a compact human label; ids with no label are
    /// shown by id. Empty history yields an empty string.
    ///
    /// When the history holds more than `max` entries the readout is clipped, so a
    /// trailing ` ← …` token is appended (e.g. `#8 ← #6 ← #4 ← #2 ← …`) to mark
    /// that older jumps exist beyond the ones shown — without it a clipped trail
    /// reads indistinguishably from an exhaustive one.
    pub(crate) fn history_summary(
        &self,
        max: usize,
        mut label: impl FnMut(u64) -> Option<String>,
    ) -> String {
        if self.history.is_empty() || max == 0 {
            return String::new();
        }
        let mut parts: Vec<String> = Vec::new();
        for id in self.history.iter().take(max) {
            parts.push(label(*id).unwrap_or_else(|| format!("#{id}")));
        }
        if self.history.len() > max {
            parts.push("\u{2026}".to_string());
        }
        parts.join(" \u{2190} ")
    }
}

#[cfg(test)]
#[path = "jump_marks_tests.rs"]
mod tests;
