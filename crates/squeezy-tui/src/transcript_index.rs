//! Local Transcript Index (§12.5.1): an in-memory index over the transcript
//! model, keyed by **stable entry ids** (`TranscriptEntry::id`), for fast
//! lookup, navigation, and filtering without re-walking the whole transcript.
//!
//! The index groups entries into a small fixed set of [`EntryCategory`] buckets
//! (conversation turns, tool calls, errors, reasoning, subagents, notes, plan
//! cards, …). Each bucket is an ordered list of entry ids, so "jump to the next
//! tool call after this one" or "how many errors are there" is an O(bucket)
//! lookup instead of a fold over every transcript entry.
//!
//! **Stable ids, never row offsets.** Like the jump-mark stack
//! (`jump_marks.rs`), every key here is an entry id, never a width-/fold-
//! dependent row coordinate. An id survives reflow (resize, streaming, collapse,
//! coalescing), so an index built before a reflow still resolves to the right
//! entry afterwards. Ids whose entry has since been dropped simply fall out on
//! the next rebuild.
//!
//! **Zero idle cost, incremental rebuild.** The index carries a `fingerprint`
//! folded over every live `(entry.id, entry.revision, category)`. The caller
//! feeds the same fingerprint each frame via [`TranscriptIndex::rebuild_if_stale`];
//! when it matches the stored one the call returns immediately and touches
//! nothing. The index is only re-walked when the transcript actually changed
//! (append, stream settle, revision bump, clear, compaction, fold toggle,
//! resume) — exactly the events that move the fingerprint. An idle session pays
//! one cheap `u64` comparison per frame and rebuilds nothing.
//!
//! This module is deliberately pure: it owns the id/category bookkeeping and
//! nothing about geometry, rendering, or input. `lib.rs` classifies each live
//! entry (reusing the same `entry_is_error` / role / `LogKind` predicates the
//! renderer and jump-nav use) into an [`IndexedEntry`] and feeds the slice in;
//! the index turns that into buckets and answers navigation queries. That keeps
//! the indexing math testable without a terminal.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// The coarse category an entry falls into for indexing/navigation. A fixed,
/// small set — one bucket per "thing the user navigates to". Ordered so
/// [`EntryCategory::ALL`] reads top-to-bottom the way the overlay lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum EntryCategory {
    /// A user message — the start of a turn.
    UserTurn,
    /// An assistant message.
    Assistant,
    /// A finalized model reasoning segment.
    Reasoning,
    /// A tool-call result (any status). Errors are *also* counted under
    /// [`EntryCategory::Error`], so a failed tool appears in both buckets.
    ToolCall,
    /// A failure surface: a failed tool result, an error/failure log, or a
    /// message whose outcome is `Failed`. Cross-cuts the other categories.
    Error,
    /// A subagent lifecycle breadcrumb.
    Subagent,
    /// A plan card.
    Plan,
    /// A `/diff` snapshot card.
    Diff,
    /// Any other log/note/operational line not classified above.
    Note,
}

impl EntryCategory {
    /// Every category, in overlay display order. Drives the summary readout and
    /// the navigation cycle. Exhaustive on purpose: a new variant must be added
    /// here or it never appears in the index summary.
    pub(crate) const ALL: &'static [EntryCategory] = &[
        EntryCategory::UserTurn,
        EntryCategory::Assistant,
        EntryCategory::Reasoning,
        EntryCategory::ToolCall,
        EntryCategory::Error,
        EntryCategory::Subagent,
        EntryCategory::Plan,
        EntryCategory::Diff,
        EntryCategory::Note,
    ];

    /// Short, screen-reader-friendly label for the summary readout. ASCII only
    /// (no glyphs) to match the rest of Squeezy's chrome.
    pub(crate) fn label(self) -> &'static str {
        match self {
            EntryCategory::UserTurn => "user turns",
            EntryCategory::Assistant => "assistant",
            EntryCategory::Reasoning => "reasoning",
            EntryCategory::ToolCall => "tool calls",
            EntryCategory::Error => "errors",
            EntryCategory::Subagent => "subagents",
            EntryCategory::Plan => "plans",
            EntryCategory::Diff => "diffs",
            EntryCategory::Note => "notes",
        }
    }
}

/// One classified transcript entry, as the caller feeds it in. `id` is the
/// stable `TranscriptEntry::id`; `revision` is its content revision (folded into
/// the fingerprint so a mutation re-indexes); `primary` is the entry's main
/// category; `is_error` flags it for the cross-cutting error bucket as well;
/// `tool_name` is the tool name for a tool-call entry (used for per-tool lookup),
/// `None` otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexedEntry {
    pub(crate) id: u64,
    pub(crate) revision: u64,
    pub(crate) primary: EntryCategory,
    pub(crate) is_error: bool,
    pub(crate) tool_name: Option<String>,
}

/// In-memory transcript index keyed by stable entry id.
///
/// Buckets map a category to the ordered list of entry ids in it (transcript
/// order). `by_id` maps an id back to its primary category for O(1) lookup, and
/// `tools` groups tool-call ids by tool name. `fingerprint` is the staleness
/// tag described in the module docs.
#[derive(Debug, Clone, Default)]
pub(crate) struct TranscriptIndex {
    buckets: HashMap<EntryCategory, Vec<u64>>,
    by_id: HashMap<u64, EntryCategory>,
    tools: HashMap<String, Vec<u64>>,
    fingerprint: u64,
    /// Total number of live entries indexed (every id appears once here even if
    /// it also lands in the cross-cutting error bucket).
    total: usize,
    /// Whether a rebuild has ever run. Distinguishes "empty transcript indexed"
    /// (fingerprint 0, built) from "never built" so a genuinely empty transcript
    /// is not re-walked every frame.
    built: bool,
}

impl TranscriptIndex {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Fold a fingerprint over a sequence of classified entries. Order- and
    /// content-sensitive: id, revision, and category all participate, so an
    /// append, a revision bump, a reorder, a category change, or a drop all move
    /// the value. Pure and standalone so the caller can compute it cheaply each
    /// frame and compare against the stored one before deciding to rebuild.
    pub(crate) fn fingerprint_of<'a>(entries: impl IntoIterator<Item = &'a IndexedEntry>) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for entry in entries {
            entry.id.hash(&mut hasher);
            entry.revision.hash(&mut hasher);
            entry.primary.hash(&mut hasher);
            entry.is_error.hash(&mut hasher);
            entry.tool_name.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Rebuild the index from `entries` **only if** `fingerprint` differs from
    /// the one captured at the last rebuild (or this is the first build). Returns
    /// `true` when a rebuild actually ran, `false` when the cached index was
    /// already current (the zero-idle-cost fast path).
    ///
    /// The caller computes `fingerprint` via [`Self::fingerprint_of`] over the
    /// same slice. Stale ids are dropped implicitly: the rebuild starts from an
    /// empty set and only the ids present in `entries` survive.
    pub(crate) fn rebuild_if_stale(&mut self, fingerprint: u64, entries: &[IndexedEntry]) -> bool {
        if self.built && fingerprint == self.fingerprint {
            return false;
        }
        self.buckets.clear();
        self.by_id.clear();
        self.tools.clear();
        self.total = 0;
        for entry in entries {
            self.buckets
                .entry(entry.primary)
                .or_default()
                .push(entry.id);
            self.by_id.insert(entry.id, entry.primary);
            if entry.is_error && entry.primary != EntryCategory::Error {
                // Cross-cut: a failed tool/message also lands in the error
                // bucket without losing its primary identity.
                self.buckets
                    .entry(EntryCategory::Error)
                    .or_default()
                    .push(entry.id);
            }
            if let Some(name) = &entry.tool_name {
                self.tools.entry(name.clone()).or_default().push(entry.id);
            }
            self.total += 1;
        }
        self.fingerprint = fingerprint;
        self.built = true;
        true
    }

    /// The stored fingerprint from the last rebuild. Test/diagnostic accessor;
    /// production compares fingerprints inside `rebuild_if_stale` rather than
    /// reading this out, so it is only consumed by the unit suite.
    #[cfg(test)]
    pub(crate) fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    /// Total number of indexed entries (each counted once under its primary
    /// category). Zero before the first build and for an empty transcript.
    pub(crate) fn total(&self) -> usize {
        self.total
    }

    /// Number of entries in `category` (the error bucket includes cross-cut
    /// failures from other categories).
    pub(crate) fn count(&self, category: EntryCategory) -> usize {
        self.buckets.get(&category).map_or(0, Vec::len)
    }

    /// The ordered entry ids in `category` (transcript order), or an empty slice
    /// when the bucket is empty.
    pub(crate) fn ids(&self, category: EntryCategory) -> &[u64] {
        self.buckets.get(&category).map_or(&[], Vec::as_slice)
    }

    /// The primary category of entry `id`, or `None` when the id is not indexed
    /// (dropped, or never present). The reverse-lookup half of the index; the
    /// related-entry navigation that consumes it lands in a follow-up, so it is
    /// exercised by the unit suite today.
    #[cfg(test)]
    pub(crate) fn category_of(&self, id: u64) -> Option<EntryCategory> {
        self.by_id.get(&id).copied()
    }

    /// The ordered tool-call ids for tool `name`, or an empty slice when no such
    /// tool appears. Case-sensitive exact match on the tool name. The per-tool
    /// lookup half of the index; the tool-filter affordance that consumes it
    /// lands in a follow-up, so it is exercised by the unit suite today.
    #[cfg(test)]
    pub(crate) fn ids_for_tool(&self, name: &str) -> &[u64] {
        self.tools.get(name).map_or(&[], Vec::as_slice)
    }

    /// The next entry id in `category` strictly after `after` (transcript
    /// order), wrapping to the first when `after` is the last or is not in the
    /// bucket. `None` only when the bucket is empty. Drives forward navigation
    /// ("jump to next tool call").
    pub(crate) fn next_in(&self, category: EntryCategory, after: Option<u64>) -> Option<u64> {
        let ids = self.ids(category);
        if ids.is_empty() {
            return None;
        }
        match after.and_then(|a| ids.iter().position(|&id| id == a)) {
            Some(pos) => Some(ids[(pos + 1) % ids.len()]),
            None => Some(ids[0]),
        }
    }

    /// The previous entry id in `category` strictly before `before` (transcript
    /// order), wrapping to the last when `before` is the first or is not in the
    /// bucket. `None` only when the bucket is empty. Drives backward navigation;
    /// the overlay walks forward via `next_in` today, so the symmetric backward
    /// step is exercised by the unit suite until a "previous in category" verb
    /// lands.
    #[cfg(test)]
    pub(crate) fn prev_in(&self, category: EntryCategory, before: Option<u64>) -> Option<u64> {
        let ids = self.ids(category);
        if ids.is_empty() {
            return None;
        }
        match before.and_then(|b| ids.iter().position(|&id| id == b)) {
            Some(0) | None => ids.last().copied(),
            Some(pos) => Some(ids[pos - 1]),
        }
    }

    /// The categories that have at least one entry, in [`EntryCategory::ALL`]
    /// order. Drives the overlay's selectable rows so empty buckets are skipped.
    pub(crate) fn non_empty_categories(&self) -> Vec<EntryCategory> {
        EntryCategory::ALL
            .iter()
            .copied()
            .filter(|&category| self.count(category) > 0)
            .collect()
    }

    /// A compact one-line summary of the populated buckets for the status line
    /// (e.g. `"3 user turns · 4 tool calls · 1 error"`), in display order. Empty
    /// string when nothing is indexed.
    pub(crate) fn summary(&self) -> String {
        let parts: Vec<String> = EntryCategory::ALL
            .iter()
            .copied()
            .filter_map(|category| {
                let n = self.count(category);
                (n > 0).then(|| format!("{n} {}", category.label()))
            })
            .collect();
        parts.join(" \u{00b7} ")
    }
}

#[cfg(test)]
#[path = "transcript_index_tests.rs"]
mod tests;
