//! What Changed Since Here? (§12.2.7): from a marked "since here" point — a
//! selected turn / transcript entry — surface the changes observed *after* it as
//! a summarized delta the user can review and jump through. The anchor is a
//! stable [`TranscriptEntry::id`](crate::TranscriptEntry) plus its chronological
//! `sequence` (transcript order); everything later than that sequence is scanned,
//! classified into a small set of high-signal [`ChangeGroupKind`]s (file edits,
//! commands/tests, errors, checkpoints/plans, approval decisions, and other tool
//! results / notes), and folded into an ordered [`ChangeSummary`] of groups, each
//! holding a list of [`ChangeItem`]s that link back to the transcript entry they
//! stand for (the quick-jump target).
//!
//! **Honest "observed since this turn" language.** The spec warns the risk is
//! overstating completeness, so the module never claims to be a full project
//! history: it reports only what the *session's own transcript* recorded after
//! the anchor. The summary header and empty-state copy say "observed since" — not
//! "all changes" — and an anchor at the very end of the session yields an honest
//! empty delta rather than inventing entries.
//!
//! **Stable ids, never row offsets.** Like the session timeline (§12.2.6), the
//! transcript index (§12.5.1), and the bookmark stack (§12.2.4), the anchor and
//! every change item are keyed by their source `TranscriptEntry::id`, never a
//! width-/fold-dependent row coordinate. An id survives reflow (resize,
//! streaming, collapse, coalescing), so a summary built before a reflow still
//! resolves to the right entry afterwards. Items whose entry was dropped fall out
//! on the next rebuild.
//!
//! **Reuses the timeline's classification.** The caller feeds the same
//! [`TimelineSource`](crate::session_timeline::TimelineSource) slice the session
//! timeline already builds (one classified record per transcript entry). This
//! module groups that slice rather than re-deriving event kinds, so "what changed
//! since here" stays in lock-step with what the timeline shows.
//!
//! **Cache by anchor sequence, invalidate on new events.** The summary carries a
//! `fingerprint` folded over every source `(id, revision, kind, status, label)`
//! plus the anchor `(id, sequence)`. The caller feeds the same fingerprint each
//! refresh via [`ChangeSummary::rebuild_if_stale`]; when it matches the stored
//! one the call returns immediately and touches nothing. The summary is only
//! re-walked when the anchor moved or the transcript actually changed (append,
//! stream settle, revision bump, clear, compaction, resume) — exactly the events
//! that move the fingerprint. An idle session pays one cheap `u64` comparison.

use crate::session_timeline::{TimelineKind, TimelineSource, TimelineStatus};
use std::hash::{Hash, Hasher};

/// The coarse change category a later event folds into on the "since here"
/// delta. A small, fixed, high-signal set — one per "thing the user reviews after
/// marking a point". Ordered so [`ChangeGroupKind::ALL`] reads top-to-bottom the
/// way a review flows (the concrete file edits first, then the commands/tests
/// that ran, then any failures, the checkpoints reached, the approval decisions
/// made, and finally other tool results / notes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ChangeGroupKind {
    /// A file edit / diff snapshot observed after the anchor.
    Edits,
    /// A command / test / tool run observed after the anchor (any status; a
    /// failed one is also surfaced under [`ChangeGroupKind::Errors`]).
    Commands,
    /// A failure surface observed after the anchor: a failed tool, a failed
    /// turn, an approval denial, or an error/failure log line.
    Errors,
    /// A plan checkpoint card observed after the anchor.
    Checkpoints,
    /// An approval / denial decision observed after the anchor.
    Decisions,
    /// Any other tool result / operational note observed after the anchor
    /// (subagent breadcrumbs, queue actions, slash echoes, …).
    Results,
}

impl ChangeGroupKind {
    /// Every group kind, in review display order. Exhaustive on purpose: a new
    /// variant must be added here or it never appears in the summary.
    pub(crate) const ALL: &'static [ChangeGroupKind] = &[
        ChangeGroupKind::Edits,
        ChangeGroupKind::Commands,
        ChangeGroupKind::Errors,
        ChangeGroupKind::Checkpoints,
        ChangeGroupKind::Decisions,
        ChangeGroupKind::Results,
    ];

    /// Short, screen-reader-friendly heading for the group. ASCII only (no
    /// glyphs) so the panel carries meaning without relying on color or a
    /// private-use codepoint.
    pub(crate) fn heading(self) -> &'static str {
        match self {
            ChangeGroupKind::Edits => "files changed",
            ChangeGroupKind::Commands => "commands & tests",
            ChangeGroupKind::Errors => "errors",
            ChangeGroupKind::Checkpoints => "checkpoints",
            ChangeGroupKind::Decisions => "decisions",
            ChangeGroupKind::Results => "tool results",
        }
    }

    /// Map one classified timeline event into the change group it belongs to.
    ///
    /// `is_error` is the event's failure cross-cut (a failed tool, failed turn,
    /// or error line); a failing event is grouped under [`ChangeGroupKind::Errors`]
    /// regardless of its base kind, because for a "what changed" review the
    /// failure is the salient fact. A non-failing event maps by its base kind:
    /// edits, plans, approval decisions, and tool/command runs each get their own
    /// group; everything else (prompts, turns, reasoning, subagents, notes) is an
    /// observed "tool result / note". Pure and total over the kind set.
    fn classify(kind: TimelineKind, is_error: bool) -> ChangeGroupKind {
        if is_error {
            return ChangeGroupKind::Errors;
        }
        match kind {
            TimelineKind::Edit => ChangeGroupKind::Edits,
            TimelineKind::Tool => ChangeGroupKind::Commands,
            TimelineKind::Plan => ChangeGroupKind::Checkpoints,
            TimelineKind::Approval => ChangeGroupKind::Decisions,
            TimelineKind::Error => ChangeGroupKind::Errors,
            // Prompts, turns, reasoning, subagents, and plain notes are not
            // themselves "changes" the user edits/reviews, but they are still
            // honest observations after the anchor, surfaced under results.
            TimelineKind::Prompt
            | TimelineKind::Turn
            | TimelineKind::Reasoning
            | TimelineKind::Subagent
            | TimelineKind::Note => ChangeGroupKind::Results,
        }
    }
}

/// Largest number of characters retained in a change-item `label`. One short
/// line: long enough to disambiguate, short enough that an item row never wraps
/// the panel. Matches the session timeline's bound so the two read consistently.
const LABEL_CAP: usize = 56;

/// Clean a caller-supplied raw label into a bounded one-line label, falling back
/// to the group's heading when the source has no usable text.
///
/// Deterministic and honest: takes the first non-empty line, collapses interior
/// whitespace runs to single spaces, trims, and caps to [`LABEL_CAP`] chars (on a
/// char boundary, appending an ellipsis when cut). A blank source yields
/// `"(<heading>)"` rather than inventing text.
fn clean_label(raw: &str, kind: ChangeGroupKind) -> String {
    let first_line = raw.lines().map(str::trim).find(|line| !line.is_empty());
    let Some(line) = first_line else {
        return format!("({})", kind.heading());
    };
    let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return format!("({})", kind.heading());
    }
    if collapsed.chars().count() <= LABEL_CAP {
        return collapsed;
    }
    let prefix: String = collapsed.chars().take(LABEL_CAP).collect();
    format!("{prefix}\u{2026}")
}

/// One change observed after the anchor. `entry_id` is the stable
/// `TranscriptEntry::id` of the entry it stands for (the quick-jump target);
/// `group` is the change group it belongs to; `failed` flags a failure so a
/// failed command/edit reads honestly even though it is grouped under
/// [`ChangeGroupKind::Errors`]; `label` is the short, bounded, deterministic,
/// secret-free label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangeItem {
    pub(crate) entry_id: u64,
    pub(crate) group: ChangeGroupKind,
    pub(crate) failed: bool,
    pub(crate) label: String,
}

/// The marked "since here" anchor: the stable id of the entry the user marked and
/// its chronological `sequence` (0-based transcript order). Everything strictly
/// after `sequence` is the delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Anchor {
    pub(crate) entry_id: u64,
    pub(crate) sequence: u32,
}

/// The computed "What Changed Since Here?" delta (§12.2.7).
///
/// `anchor` is the marked point (`None` = nothing marked yet, the resting state).
/// `items` is the ordered, flattened list of changes observed after the anchor,
/// grouped by [`ChangeGroupKind::ALL`] order and chronological within each group
/// (so the rendered list reads as a review checklist). `fingerprint` is the
/// staleness tag described in the module docs; `built` distinguishes "empty delta
/// built" from "never built" so a genuinely empty delta is not re-walked every
/// refresh.
#[derive(Debug, Clone, Default)]
pub(crate) struct ChangeSummary {
    anchor: Option<Anchor>,
    items: Vec<ChangeItem>,
    fingerprint: u64,
    built: bool,
}

impl ChangeSummary {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The active anchor, or `None` when nothing has been marked yet.
    pub(crate) fn anchor(&self) -> Option<Anchor> {
        self.anchor
    }

    /// Whether a "since here" point has been marked.
    pub(crate) fn has_anchor(&self) -> bool {
        self.anchor.is_some()
    }

    /// Fold a staleness fingerprint over the sources and the prospective anchor.
    /// Order- and content-sensitive: every source's id, revision, kind, error
    /// flag, and label source participate, plus the anchor's id and sequence, so
    /// an append, a revision bump, a re-edit, a status flip, or a move of the
    /// anchor all change the value. Pure and standalone so the caller can compute
    /// it cheaply each refresh and compare before deciding to recompute.
    pub(crate) fn fingerprint_of<'a>(
        anchor: Option<Anchor>,
        sources: impl IntoIterator<Item = &'a TimelineSource>,
    ) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match anchor {
            Some(a) => {
                1u8.hash(&mut hasher);
                a.entry_id.hash(&mut hasher);
                a.sequence.hash(&mut hasher);
            }
            None => 0u8.hash(&mut hasher),
        }
        for source in sources {
            source.id.hash(&mut hasher);
            source.revision.hash(&mut hasher);
            source.kind.hash(&mut hasher);
            source.is_error.hash(&mut hasher);
            source.raw_label.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Mark (or re-mark) the "since here" point at `anchor`. Clears the cached
    /// delta so the next [`Self::rebuild_if_stale`] recomputes against the new
    /// anchor (the fingerprint also moves, so the rebuild is not skipped).
    pub(crate) fn set_anchor(&mut self, anchor: Anchor) {
        self.anchor = Some(anchor);
        self.built = false;
    }

    /// Clear the marked point and the cached delta. After this
    /// [`Self::has_anchor`] is `false` and the delta is empty. Exercised by the
    /// unit suite; production re-marks rather than clears, so this is `cfg(test)`
    /// to stay lint-clean on every platform.
    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.anchor = None;
        self.items.clear();
        self.fingerprint = 0;
        self.built = false;
    }

    /// Recompute the delta from `sources` **only if** `fingerprint` differs from
    /// the one captured at the last rebuild (or this is the first build). Returns
    /// `true` when a recompute actually ran, `false` when the cached delta was
    /// already current (the zero-idle-cost fast path).
    ///
    /// The caller computes `fingerprint` via [`Self::fingerprint_of`] over the
    /// same anchor + slice. With no anchor the delta is empty. The walk keeps only
    /// sources strictly *after* the anchor's sequence, classifies each into its
    /// change group, and emits items grouped by [`ChangeGroupKind::ALL`] order
    /// (chronological within each group). Stale ids are dropped implicitly: the
    /// rebuild starts empty and only sources present in `sources` survive.
    pub(crate) fn rebuild_if_stale(
        &mut self,
        fingerprint: u64,
        sources: &[TimelineSource],
    ) -> bool {
        if self.built && fingerprint == self.fingerprint {
            return false;
        }
        self.items.clear();
        if let Some(anchor) = self.anchor {
            // Classify every later source once, preserving chronological order
            // within each group by walking the slice in order per group.
            for &group in ChangeGroupKind::ALL {
                for source in sources
                    .iter()
                    .enumerate()
                    .filter(|(seq, _)| *seq as u32 > anchor.sequence)
                    .map(|(_, source)| source)
                {
                    if ChangeGroupKind::classify(source.kind, source.is_error) != group {
                        continue;
                    }
                    self.items.push(ChangeItem {
                        entry_id: source.id,
                        group,
                        failed: source.is_error
                            || matches!(event_status(source), TimelineStatus::Failed),
                        label: clean_label(&source.raw_label, group),
                    });
                }
            }
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

    /// Every change item in review order (grouped, chronological within a group).
    pub(crate) fn items(&self) -> &[ChangeItem] {
        &self.items
    }

    /// Total number of change items observed after the anchor.
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the delta is empty (no anchor, or the anchor is at/after the last
    /// event so nothing changed since).
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The change item at flattened index `index`, or `None` when out of range.
    /// Drives the panel's selectable rows.
    pub(crate) fn get(&self, index: usize) -> Option<&ChangeItem> {
        self.items.get(index)
    }

    /// Number of items in `group` (across the whole delta).
    pub(crate) fn count_of(&self, group: ChangeGroupKind) -> usize {
        self.items.iter().filter(|i| i.group == group).count()
    }

    /// Number of failed items (any group whose item is flagged `failed`).
    pub(crate) fn failed_count(&self) -> usize {
        self.items.iter().filter(|i| i.failed).count()
    }

    /// The groups that have at least one item, in [`ChangeGroupKind::ALL`] order.
    /// The render walks the flattened list and emits a heading per group inline, so
    /// this is exercised by the unit suite (asserting group presence/order) rather
    /// than production — `cfg(test)` to stay lint-clean on every platform.
    #[cfg(test)]
    pub(crate) fn present_groups(&self) -> Vec<ChangeGroupKind> {
        ChangeGroupKind::ALL
            .iter()
            .copied()
            .filter(|&group| self.items.iter().any(|i| i.group == group))
            .collect()
    }

    /// The flattened index of the next item strictly after `after` (wrapping to
    /// the first when `after` is the last or `None`). `None` only when the delta
    /// is empty. Drives forward review navigation.
    pub(crate) fn next_index(&self, after: Option<usize>) -> Option<usize> {
        let count = self.items.len();
        if count == 0 {
            return None;
        }
        match after {
            Some(i) if i + 1 < count => Some(i + 1),
            Some(_) => Some(0),
            None => Some(0),
        }
    }

    /// The flattened index of the previous item strictly before `before`
    /// (wrapping to the last). `None` only when the delta is empty. The overlay's
    /// `p`/Up verb clamps toward the start (it never wraps), so the wrapping
    /// backward walk is exercised by the unit suite — `cfg(test)` to stay
    /// lint-clean on every platform until a wrapping "previous change" verb lands.
    #[cfg(test)]
    pub(crate) fn prev_index(&self, before: Option<usize>) -> Option<usize> {
        let count = self.items.len();
        if count == 0 {
            return None;
        }
        match before {
            Some(0) | None => Some(count - 1),
            Some(i) if i <= count => Some(i - 1),
            Some(_) => Some(count - 1),
        }
    }

    /// A compact one-line summary of the delta for the status line / panel
    /// header, e.g. `"3 changes \u{00b7} 1 files changed \u{00b7} 1 commands &
    /// tests \u{00b7} 1 errors \u{00b7} 1 failed"`. Empty string when the delta is
    /// empty. The honest "observed since" framing lives in the caller's header
    /// copy; this counts only what the transcript recorded.
    pub(crate) fn summary(&self) -> String {
        if self.items.is_empty() {
            return String::new();
        }
        let total = self.items.len();
        let total_word = if total == 1 { "change" } else { "changes" };
        let mut parts = vec![format!("{total} {total_word}")];
        for group in ChangeGroupKind::ALL.iter().copied() {
            let n = self.count_of(group);
            if n > 0 {
                parts.push(format!("{n} {}", group.heading()));
            }
        }
        let failed = self.failed_count();
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        parts.join(" \u{00b7} ")
    }
}

/// The [`TimelineStatus`] a source maps to (error wins over pending), reused so a
/// failure flag here matches what the session timeline paints. Kept private — the
/// summary only needs the failed/not distinction.
fn event_status(source: &TimelineSource) -> TimelineStatus {
    if source.is_error {
        TimelineStatus::Failed
    } else if source.is_pending {
        TimelineStatus::Pending
    } else {
        TimelineStatus::Ok
    }
}

#[cfg(test)]
#[path = "change_summary_tests.rs"]
mod tests;
