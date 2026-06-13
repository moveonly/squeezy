//! Semantic Turn Outline (§12.2.1): a navigable structural map of a long
//! session — user prompts, assistant answers, tool calls, errors, reasoning,
//! plans, diffs, and subagent breadcrumbs — shown as a jump-list/overlay.
//! Selecting a node scrolls the main view to the logical transcript row it
//! stands for.
//!
//! An [`OutlineNode`] carries the stable [`TranscriptEntry::id`](crate::TranscriptEntry)
//! it lives on (the quick-jump target — `entry_id`), a deterministic local
//! [`OutlineKind`], a [`OutlineStatus`] (ok / failed), and a short honest
//! `title` generated from the entry's own text (first line of a message, the
//! tool name + status, the error's first line, the plan/diff summary). The
//! module is deliberately pure: it owns the node/navigation bookkeeping and
//! nothing about geometry, rendering, or input. `lib.rs` classifies each live
//! entry into an [`OutlineEntry`] (reusing the same role / `LogKind` /
//! `entry_is_error` predicates the index and renderer use) and feeds the slice
//! in; this module turns those facts into an ordered outline and answers
//! list/navigation queries. That keeps the outline math testable without a
//! terminal.
//!
//! **Stable ids, never row offsets.** Like the transcript index (§12.5.1), the
//! jump-mark stack (`jump_marks.rs`), and the health markers (§12.5.7), every
//! node is keyed by its source `TranscriptEntry::id`, never a width-/fold-
//! dependent row coordinate. An id survives reflow (resize, streaming, collapse,
//! coalescing), so an outline built before a reflow still resolves to the right
//! entry afterwards. Ids whose entry was dropped fall out on the next rebuild —
//! exactly the "jumps survive resize and fold changes" the spec asks for.
//!
//! **Honest deterministic labels, never fake summaries.** The spec warns that
//! weak titles are noise and to "prefer honest deterministic labels over fake
//! summaries". So a title is derived only from the entry's own first content
//! line (or its tool name / kind), bounded to one short line; a title-less entry
//! falls back to its kind label (e.g. `"(assistant)"`) rather than inventing
//! text. No model is consulted and no content is re-summarised.
//!
//! **Zero idle cost, incremental rebuild.** The outline carries a `fingerprint`
//! folded over every entry `(id, revision)`. The caller feeds the same
//! fingerprint each refresh via [`OutlineIndex::rebuild_if_stale`]; when it
//! matches the stored one the call returns immediately and touches nothing. The
//! outline is only re-walked when the transcript actually changed (append,
//! stream settle, revision bump, clear, compaction, fold/filter toggle, resume)
//! — exactly the events that move the fingerprint. An idle session pays one
//! cheap `u64` comparison per refresh and rebuilds nothing.

use std::hash::{Hash, Hasher};

/// The coarse kind a transcript entry maps to in the outline — a small, fixed
/// set, one per "section the user navigates between". Ordered so
/// [`OutlineKind::ALL`] reads top-to-bottom the way a turn flows (the user
/// prompt, the model's reasoning/answer, the tools it ran, any error, then
/// plan/diff/subagent breadcrumbs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum OutlineKind {
    /// A user message — the head of a turn.
    UserTurn,
    /// An assistant message — the model's answer.
    Assistant,
    /// A finalized model reasoning segment.
    Reasoning,
    /// A tool-call result (any status; a failed one is also flagged via
    /// [`OutlineStatus::Failed`]).
    ToolRun,
    /// A failure surface that is not itself a tool/turn: an error/failure log
    /// line.
    Error,
    /// A subagent lifecycle breadcrumb.
    Subagent,
    /// A plan card.
    Plan,
    /// A `/diff` snapshot card.
    Diff,
    /// Any other note / operational line.
    Note,
}

impl OutlineKind {
    /// Every kind, in outline display order. Exhaustive on purpose: a new
    /// variant must be added here or it never appears in the kind summary.
    pub(crate) const ALL: &'static [OutlineKind] = &[
        OutlineKind::UserTurn,
        OutlineKind::Assistant,
        OutlineKind::Reasoning,
        OutlineKind::ToolRun,
        OutlineKind::Error,
        OutlineKind::Subagent,
        OutlineKind::Plan,
        OutlineKind::Diff,
        OutlineKind::Note,
    ];

    /// Short, screen-reader-friendly label for the node's kind tag and the
    /// title-less fallback. ASCII only (no glyphs) so the outline carries
    /// meaning without relying on color or a private-use codepoint.
    pub(crate) fn label(self) -> &'static str {
        match self {
            OutlineKind::UserTurn => "user",
            OutlineKind::Assistant => "assistant",
            OutlineKind::Reasoning => "reasoning",
            OutlineKind::ToolRun => "tool",
            OutlineKind::Error => "error",
            OutlineKind::Subagent => "subagent",
            OutlineKind::Plan => "plan",
            OutlineKind::Diff => "diff",
            OutlineKind::Note => "note",
        }
    }
}

/// Whether the node's entry is in a failure state. Cross-cuts the kind (a failed
/// tool is `ToolRun` + `Failed`), so the outline can flag dead turns/tools
/// without losing their primary section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum OutlineStatus {
    /// The entry completed normally (or carries no failure signal).
    Ok,
    /// The entry is a failure: a failed tool, a failed turn, or an error line.
    Failed,
}

impl OutlineStatus {
    /// ASCII label for the readout.
    pub(crate) fn label(self) -> &'static str {
        match self {
            OutlineStatus::Ok => "ok",
            OutlineStatus::Failed => "failed",
        }
    }
}

/// One classified transcript entry, as the caller feeds it in. `id` is the
/// stable `TranscriptEntry::id`; `revision` is its content revision (folded into
/// the staleness fingerprint so a mutation re-outlines); `kind` is the entry's
/// section; `is_error` flags it for [`OutlineStatus::Failed`]; `raw_title` is
/// the entry's own first content line / tool name (already bounded and
/// secret-free), or empty when the entry has no usable text (a title-less
/// entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutlineEntry {
    pub(crate) id: u64,
    pub(crate) revision: u64,
    pub(crate) kind: OutlineKind,
    pub(crate) is_error: bool,
    /// The raw, caller-supplied title source (first content line, tool name,
    /// …). Cleaned + bounded + deterministically fallen-back here.
    pub(crate) raw_title: String,
}

/// One node of the outline (§12.2.1). `entry_id` is the stable
/// `TranscriptEntry::id` of the entry it stands for (the quick-jump target);
/// `kind` is the section; `status` is ok/failed; `title` is the short, bounded,
/// deterministic, secret-free label; `full_title` is the same label *before*
/// the [`TITLE_CAP`] truncation, retained so the overlay can reveal the cut
/// text on selection rather than forcing the user to leave the outline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutlineNode {
    pub(crate) entry_id: u64,
    pub(crate) kind: OutlineKind,
    pub(crate) status: OutlineStatus,
    pub(crate) title: String,
    pub(crate) full_title: String,
}

impl OutlineNode {
    /// Whether [`title`](Self::title) was truncated from
    /// [`full_title`](Self::full_title) — the cue for the overlay to offer a
    /// reveal of the cut text.
    pub(crate) fn is_truncated(&self) -> bool {
        self.title != self.full_title
    }
}

/// Largest number of characters retained in a node `title`. One short line: long
/// enough to disambiguate, short enough that a node row never wraps the overlay.
const TITLE_CAP: usize = 60;

/// Clean a caller-supplied raw title into a bounded one-line label, falling back
/// to the kind's parenthesised label when the source has no usable text.
///
/// Deterministic and honest: takes the first non-empty line, collapses interior
/// whitespace runs to single spaces, trims, and caps to [`TITLE_CAP`] chars (on
/// a char boundary, appending an ellipsis when cut). A blank source yields
/// `"(kind)"` rather than inventing text — the spec's "prefer honest
/// deterministic labels over fake summaries".
pub(crate) fn clean_title(raw: &str, kind: OutlineKind) -> String {
    let full = full_title(raw, kind);
    if full.chars().count() <= TITLE_CAP {
        return full;
    }
    let prefix: String = full.chars().take(TITLE_CAP).collect();
    format!("{prefix}\u{2026}")
}

/// The collapsed one-line title *before* the [`TITLE_CAP`] truncation that
/// [`clean_title`] applies. Same honest derivation (first non-blank line,
/// interior whitespace collapsed, kind fallback) but uncapped, so the overlay
/// can recover the full text the truncated label dropped.
fn full_title(raw: &str, kind: OutlineKind) -> String {
    // First non-blank line only — a node label is one line.
    let first_line = raw.lines().map(str::trim).find(|line| !line.is_empty());
    let Some(line) = first_line else {
        return format!("({})", kind.label());
    };
    // Collapse interior whitespace runs (including tabs) to a single space so a
    // padded source renders as a tidy single line.
    let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return format!("({})", kind.label());
    }
    collapsed
}

/// Build the outline node for one classified entry. Pure and standalone so it is
/// the unit-testable heart of the feature: the title is cleaned/bounded, the
/// status folds the error flag, and the kind passes through. Always emits
/// exactly one node (the outline lists every navigable entry).
pub(crate) fn node_for_entry(entry: &OutlineEntry) -> OutlineNode {
    let status = if entry.is_error {
        OutlineStatus::Failed
    } else {
        OutlineStatus::Ok
    };
    OutlineNode {
        entry_id: entry.id,
        kind: entry.kind,
        status,
        title: clean_title(&entry.raw_title, entry.kind),
        full_title: full_title(&entry.raw_title, entry.kind),
    }
}

/// The computed Semantic Turn Outline over the transcript's entries (§12.2.1).
///
/// `nodes` is the ordered list of outline nodes (transcript order). `fingerprint`
/// is the staleness tag described in the module docs; `built` distinguishes
/// "empty transcript outlined" from "never built" so a genuinely empty
/// transcript is not re-walked every refresh.
#[derive(Debug, Clone, Default)]
pub(crate) struct OutlineIndex {
    nodes: Vec<OutlineNode>,
    fingerprint: u64,
    built: bool,
}

impl OutlineIndex {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Fold a staleness fingerprint over the entries. Order- and
    /// content-sensitive: id, revision, kind, error flag, and title source all
    /// participate, so an append, a revision bump, a reorder, a re-edit, or a
    /// drop all move the value. Pure and standalone so the caller can compute it
    /// cheaply each refresh and compare before deciding to recompute.
    pub(crate) fn fingerprint_of<'a>(entries: impl IntoIterator<Item = &'a OutlineEntry>) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for entry in entries {
            entry.id.hash(&mut hasher);
            entry.revision.hash(&mut hasher);
            entry.kind.hash(&mut hasher);
            entry.is_error.hash(&mut hasher);
            entry.raw_title.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Recompute the outline from `entries` **only if** `fingerprint` differs
    /// from the one captured at the last rebuild (or this is the first build).
    /// Returns `true` when a recompute actually ran, `false` when the cached
    /// outline was already current (the zero-idle-cost fast path).
    ///
    /// The caller computes `fingerprint` via [`Self::fingerprint_of`] over the
    /// same slice. Stale ids are dropped implicitly: the rebuild starts from an
    /// empty set and only the entries present in `entries` survive.
    pub(crate) fn rebuild_if_stale(&mut self, fingerprint: u64, entries: &[OutlineEntry]) -> bool {
        if self.built && fingerprint == self.fingerprint {
            return false;
        }
        self.nodes.clear();
        self.nodes.reserve(entries.len());
        for entry in entries {
            self.nodes.push(node_for_entry(entry));
        }
        self.fingerprint = fingerprint;
        self.built = true;
        true
    }

    /// The stored staleness fingerprint from the last rebuild. Test/diagnostic
    /// accessor; production compares inside `rebuild_if_stale`.
    #[cfg(test)]
    pub(crate) fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    /// The outline nodes in transcript order.
    pub(crate) fn nodes(&self) -> &[OutlineNode] {
        &self.nodes
    }

    /// Number of outline nodes.
    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the outline has any nodes.
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The node at list index `index`, or `None` when out of range.
    pub(crate) fn get(&self, index: usize) -> Option<&OutlineNode> {
        self.nodes.get(index)
    }

    /// Number of nodes of `kind`.
    pub(crate) fn count_of(&self, kind: OutlineKind) -> usize {
        self.nodes.iter().filter(|n| n.kind == kind).count()
    }

    /// Number of failed nodes (any kind whose status is [`OutlineStatus::Failed`]).
    pub(crate) fn failed_count(&self) -> usize {
        self.nodes
            .iter()
            .filter(|n| n.status == OutlineStatus::Failed)
            .count()
    }

    /// The list index of the next node strictly after `after` (wrapping to the
    /// first when `after` is the last or `None`). `None` only when there are no
    /// nodes. Drives forward quick-jump navigation.
    pub(crate) fn next_index(&self, after: Option<usize>) -> Option<usize> {
        if self.nodes.is_empty() {
            return None;
        }
        match after {
            Some(i) if i + 1 < self.nodes.len() => Some(i + 1),
            // Last (or out of range): wrap to the first.
            Some(_) => Some(0),
            None => Some(0),
        }
    }

    /// The list index of the previous node strictly before `before` (wrapping to
    /// the last). `None` only when there are no nodes. Drives backward quick-jump
    /// navigation; the overlay walks forward on Enter today, so this is exercised
    /// by the unit suite until a "previous node" verb lands.
    #[cfg(test)]
    pub(crate) fn prev_index(&self, before: Option<usize>) -> Option<usize> {
        if self.nodes.is_empty() {
            return None;
        }
        match before {
            Some(0) | None => Some(self.nodes.len() - 1),
            Some(i) if i <= self.nodes.len() => Some(i - 1),
            // Out of range: wrap to the last.
            Some(_) => Some(self.nodes.len() - 1),
        }
    }

    /// A compact one-line summary of the outline for the status line / overlay
    /// header, e.g. `"6 sections \u{00b7} 2 user \u{00b7} 2 tool \u{00b7} 1
    /// error \u{00b7} 1 failed"`. Empty string when the outline is empty.
    pub(crate) fn summary(&self) -> String {
        if self.nodes.is_empty() {
            return String::new();
        }
        let total = self.nodes.len();
        let total_word = if total == 1 { "section" } else { "sections" };
        let mut parts = vec![format!("{total} {total_word}")];
        for kind in OutlineKind::ALL.iter().copied() {
            let n = self.count_of(kind);
            if n > 0 {
                parts.push(format!("{n} {}", kind.label()));
            }
        }
        let failed = self.failed_count();
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        parts.join(" \u{00b7} ")
    }
}

#[cfg(test)]
#[path = "turn_outline_tests.rs"]
mod tests;
