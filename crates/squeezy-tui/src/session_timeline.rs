//! Session Timeline (§12.2.6): a compact, chronological event view of the
//! session — prompts, turns, tool runs, approvals, edits, errors, and other
//! high-signal state changes — rendered as a rail/list and grouped by turn, so
//! the whole arc of a long session reads at a glance. Selecting a point scrolls
//! the main view to the transcript row that event stands for.
//!
//! A [`TimelineEvent`] carries the stable [`TranscriptEntry::id`](crate::TranscriptEntry)
//! it lives on (the quick-jump target — `entry_id`), a monotonic `sequence`
//! (its chronological position, transcript order), an **optional** `timestamp`
//! (the spec's "missing timestamps" case is first-class — `None` simply omits
//! the time column), a deterministic local [`TimelineKind`], a
//! [`TimelineStatus`] (ok / failed / pending), and the `turn` it belongs to
//! (events are grouped by turn). The module is deliberately pure: it owns the
//! event/navigation/filter bookkeeping and nothing about geometry, rendering, or
//! input. `lib.rs` classifies each live entry into a [`TimelineSource`] (reusing
//! the same role / `LogKind` / `entry_is_error` predicates the index, outline,
//! and renderer use) and feeds the slice in; this module turns those facts into
//! an ordered, filterable timeline and answers list/navigation queries. That
//! keeps the timeline math testable without a terminal.
//!
//! **Stable ids, never row offsets.** Like the transcript index (§12.5.1), the
//! turn outline (§12.2.1), and the jump-mark stack (`jump_marks.rs`), every
//! event is keyed by its source `TranscriptEntry::id`, never a width-/fold-
//! dependent row coordinate. An id survives reflow (resize, streaming, collapse,
//! coalescing), so a timeline built before a reflow still resolves to the right
//! entry afterwards. Ids whose entry was dropped fall out on the next rebuild.
//!
//! **High-signal events and honest labels.** The spec warns the risk is noise,
//! so the timeline starts from high-signal events (prompts, turns, tool runs,
//! approvals, edits, errors) and a [kind] filter lets the user narrow further.
//! A label is derived only from the entry's own first content line / tool name /
//! kind, bounded to one short line; a label-less event falls back to its kind
//! label rather than inventing text. No model is consulted.
//!
//! **Zero idle cost, incremental rebuild.** The timeline carries a `fingerprint`
//! folded over every event `(id, revision, kind, status, turn, label)`. The
//! caller feeds the same fingerprint each refresh via
//! [`SessionTimeline::rebuild_if_stale`]; when it matches the stored one the
//! call returns immediately and touches nothing. The timeline is only re-walked
//! when the transcript actually changed (append, stream settle, revision bump,
//! clear, compaction, fold/filter toggle, resume) — exactly the events that move
//! the fingerprint. An idle session pays one cheap `u64` comparison per refresh
//! and rebuilds nothing.

use std::hash::{Hash, Hasher};

/// The coarse kind a session event maps to on the timeline — a small, fixed set
/// of high-signal kinds, one per "thing that happened the user navigates to".
/// Ordered so [`TimelineKind::ALL`] reads top-to-bottom the way a turn flows
/// (the prompt, the model's reasoning/answer, the tools it ran, any approval,
/// any edit, then errors and other notes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TimelineKind {
    /// A user prompt — the head of a turn.
    Prompt,
    /// An assistant answer — the model's turn output.
    Turn,
    /// A finalized model reasoning segment.
    Reasoning,
    /// A tool run (any status; a failed one is also flagged via
    /// [`TimelineStatus::Failed`]).
    Tool,
    /// An approval / denial decision surfaced as a transcript note.
    Approval,
    /// A file edit / diff snapshot.
    Edit,
    /// A plan checkpoint card.
    Plan,
    /// A failure surface that is not itself a tool/turn: an error/failure log
    /// line.
    Error,
    /// A subagent lifecycle breadcrumb.
    Subagent,
    /// Any other operational note / state change (queue action, slash echo, …).
    Note,
}

impl TimelineKind {
    /// Every kind, in timeline display order. Exhaustive on purpose: a new
    /// variant must be added here or it never appears in the summary / filter
    /// cycle.
    pub(crate) const ALL: &'static [TimelineKind] = &[
        TimelineKind::Prompt,
        TimelineKind::Turn,
        TimelineKind::Reasoning,
        TimelineKind::Tool,
        TimelineKind::Approval,
        TimelineKind::Edit,
        TimelineKind::Plan,
        TimelineKind::Error,
        TimelineKind::Subagent,
        TimelineKind::Note,
    ];

    /// Short, screen-reader-friendly label for the event's kind tag and the
    /// label-less fallback. ASCII only (no glyphs) so the timeline carries
    /// meaning without relying on color or a private-use codepoint.
    pub(crate) fn label(self) -> &'static str {
        match self {
            TimelineKind::Prompt => "prompt",
            TimelineKind::Turn => "turn",
            TimelineKind::Reasoning => "reasoning",
            TimelineKind::Tool => "tool",
            TimelineKind::Approval => "approval",
            TimelineKind::Edit => "edit",
            TimelineKind::Plan => "plan",
            TimelineKind::Error => "error",
            TimelineKind::Subagent => "subagent",
            TimelineKind::Note => "note",
        }
    }
}

/// Whether a timeline event is ok, failed, or still in flight. Cross-cuts the
/// kind (a failed tool is `Tool` + `Failed`), so the timeline can flag dead
/// turns/tools and streaming-in-progress turns without losing their primary
/// kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TimelineStatus {
    /// The event completed normally (or carries no failure signal).
    Ok,
    /// The event is a failure: a failed tool, a failed turn, or an error line.
    Failed,
    /// The event is still in flight (a streaming turn that has not settled).
    Pending,
}

impl TimelineStatus {
    /// ASCII label for the readout.
    pub(crate) fn label(self) -> &'static str {
        match self {
            TimelineStatus::Ok => "ok",
            TimelineStatus::Failed => "failed",
            TimelineStatus::Pending => "pending",
        }
    }
}

/// One classified transcript entry, as the caller feeds it in. `id` is the
/// stable `TranscriptEntry::id`; `revision` is its content revision (folded into
/// the staleness fingerprint so a mutation re-builds); `kind` is the event kind;
/// `is_error` flags it for [`TimelineStatus::Failed`]; `is_pending` flags an
/// in-flight (streaming, not-yet-settled) turn for [`TimelineStatus::Pending`];
/// `turn` is the 1-based turn ordinal the event belongs to (events are grouped
/// by turn); `timestamp` is the optional event time in seconds since the session
/// start (or epoch) — `None` when the source carries no timestamp (the spec's
/// "missing timestamps" case); `raw_label` is the entry's own first content line
/// / tool name (already bounded and secret-free), or empty when the entry has no
/// usable text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TimelineSource {
    pub(crate) id: u64,
    pub(crate) revision: u64,
    pub(crate) kind: TimelineKind,
    pub(crate) is_error: bool,
    pub(crate) is_pending: bool,
    pub(crate) turn: u32,
    pub(crate) timestamp: Option<u64>,
    /// The raw, caller-supplied label source (first content line, tool name,
    /// …). Cleaned + bounded + deterministically fallen-back here.
    pub(crate) raw_label: String,
}

/// One event on the timeline (§12.2.6). `entry_id` is the stable
/// `TranscriptEntry::id` of the entry it stands for (the quick-jump target);
/// `sequence` is its chronological position (transcript order, 0-based); `kind`
/// is the event kind; `status` is ok/failed/pending; `turn` is the 1-based turn
/// it belongs to; `timestamp` is the optional event time; `label` is the short,
/// bounded, deterministic, secret-free label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TimelineEvent {
    pub(crate) entry_id: u64,
    pub(crate) sequence: u32,
    pub(crate) kind: TimelineKind,
    pub(crate) status: TimelineStatus,
    pub(crate) turn: u32,
    pub(crate) timestamp: Option<u64>,
    pub(crate) label: String,
}

impl TimelineEvent {
    /// The event's timestamp rendered as a compact `m:ss` clock relative to the
    /// session start (rolling up to `h:mm:ss` once an hour elapses so the
    /// minutes field never runs away), or `"--:--"` when the source carried no
    /// timestamp (the honest "missing timestamp" rendering). Pure so the time
    /// column is unit-testable without a terminal.
    pub(crate) fn clock(&self) -> String {
        match self.timestamp {
            Some(secs) => {
                let seconds = secs % 60;
                if secs >= 3600 {
                    let hours = secs / 3600;
                    let minutes = (secs % 3600) / 60;
                    format!("{hours}:{minutes:02}:{seconds:02}")
                } else {
                    let minutes = secs / 60;
                    format!("{minutes}:{seconds:02}")
                }
            }
            None => "--:--".to_string(),
        }
    }
}

/// Largest number of characters retained in an event `label`. One short line:
/// long enough to disambiguate, short enough that an event row never wraps the
/// overlay.
const LABEL_CAP: usize = 56;

/// Clean a caller-supplied raw label into a bounded one-line label, falling back
/// to the kind's parenthesised label when the source has no usable text.
///
/// Deterministic and honest: takes the first non-empty line, collapses interior
/// whitespace runs to single spaces, trims, and caps to [`LABEL_CAP`] chars (on
/// a char boundary, appending an ellipsis when cut). A blank source yields
/// `"(kind)"` rather than inventing text.
pub(crate) fn clean_label(raw: &str, kind: TimelineKind) -> String {
    let first_line = raw.lines().map(str::trim).find(|line| !line.is_empty());
    let Some(line) = first_line else {
        return format!("({})", kind.label());
    };
    let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return format!("({})", kind.label());
    }
    if collapsed.chars().count() <= LABEL_CAP {
        return collapsed;
    }
    let prefix: String = collapsed.chars().take(LABEL_CAP).collect();
    format!("{prefix}\u{2026}")
}

/// Build the timeline event for one classified source. Pure and standalone so it
/// is the unit-testable heart of the feature: the label is cleaned/bounded, the
/// status folds the error/pending flags (error wins over pending — a failed
/// in-flight turn reads as failed), and the kind/turn/timestamp pass through.
/// `sequence` is the event's position in chronological order. Always emits
/// exactly one event (the timeline lists every navigable entry).
pub(crate) fn event_for_source(source: &TimelineSource, sequence: u32) -> TimelineEvent {
    let status = if source.is_error {
        TimelineStatus::Failed
    } else if source.is_pending {
        TimelineStatus::Pending
    } else {
        TimelineStatus::Ok
    };
    TimelineEvent {
        entry_id: source.id,
        sequence,
        kind: source.kind,
        status,
        turn: source.turn,
        timestamp: source.timestamp,
        label: clean_label(&source.raw_label, source.kind),
    }
}

/// The computed Session Timeline over the transcript's entries (§12.2.6).
///
/// `events` is the ordered list of every event (chronological / transcript
/// order). `filter` is the active kind filter (`None` = show all); the rendered
/// list is the events passing it. `fingerprint` is the staleness tag described
/// in the module docs; `built` distinguishes "empty transcript built" from
/// "never built" so a genuinely empty transcript is not re-walked every refresh.
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionTimeline {
    events: Vec<TimelineEvent>,
    filter: Option<TimelineKind>,
    fingerprint: u64,
    built: bool,
}

impl SessionTimeline {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Fold a staleness fingerprint over the sources. Order- and
    /// content-sensitive: id, revision, kind, error/pending flags, turn, and
    /// label source all participate, so an append, a revision bump, a reorder, a
    /// re-edit, a turn change, or a status flip all move the value. Pure and
    /// standalone so the caller can compute it cheaply each refresh and compare
    /// before deciding to recompute. The active filter is *not* folded in —
    /// filtering is a cheap view operation that never needs a rebuild.
    pub(crate) fn fingerprint_of<'a>(sources: impl IntoIterator<Item = &'a TimelineSource>) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for source in sources {
            source.id.hash(&mut hasher);
            source.revision.hash(&mut hasher);
            source.kind.hash(&mut hasher);
            source.is_error.hash(&mut hasher);
            source.is_pending.hash(&mut hasher);
            source.turn.hash(&mut hasher);
            source.timestamp.hash(&mut hasher);
            source.raw_label.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Recompute the timeline from `sources` **only if** `fingerprint` differs
    /// from the one captured at the last rebuild (or this is the first build).
    /// Returns `true` when a recompute actually ran, `false` when the cached
    /// timeline was already current (the zero-idle-cost fast path).
    ///
    /// The caller computes `fingerprint` via [`Self::fingerprint_of`] over the
    /// same slice. Stale ids are dropped implicitly: the rebuild starts from an
    /// empty set and only the sources present in `sources` survive. The active
    /// filter is preserved across a rebuild (it is a view setting, not data).
    pub(crate) fn rebuild_if_stale(
        &mut self,
        fingerprint: u64,
        sources: &[TimelineSource],
    ) -> bool {
        if self.built && fingerprint == self.fingerprint {
            return false;
        }
        self.events.clear();
        self.events.reserve(sources.len());
        for (sequence, source) in sources.iter().enumerate() {
            self.events.push(event_for_source(source, sequence as u32));
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

    /// Every event in chronological order, regardless of the active filter. The
    /// production overlay paints the *visible* (filtered) slice via [`Self::visible`];
    /// the unfiltered list is read by the unit suite to assert order/grouping.
    #[cfg(test)]
    pub(crate) fn events(&self) -> &[TimelineEvent] {
        &self.events
    }

    /// Total number of events (unfiltered).
    pub(crate) fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the timeline has any events (unfiltered).
    pub(crate) fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The active kind filter, or `None` when every event is shown.
    pub(crate) fn filter(&self) -> Option<TimelineKind> {
        self.filter
    }

    /// Cycle the active kind filter forward: `None` (show all) → each
    /// **non-empty** kind in [`TimelineKind::ALL`] order → back to `None`. Only
    /// kinds that actually have an event are offered, so the cycle never lands on
    /// a filter that would show nothing. A no-op (stays `None`) when the timeline
    /// is empty.
    pub(crate) fn cycle_filter(&mut self) {
        let present = self.present_kinds();
        if present.is_empty() {
            self.filter = None;
            return;
        }
        self.filter = match self.filter {
            None => present.first().copied(),
            Some(current) => {
                let pos = present.iter().position(|&k| k == current);
                match pos {
                    // Last present kind wraps back to "show all".
                    Some(i) if i + 1 < present.len() => present.get(i + 1).copied(),
                    Some(_) => None,
                    // Current filter is no longer present (its events dropped):
                    // fall back to "show all" rather than a stale kind.
                    None => None,
                }
            }
        };
    }

    /// The kinds that have at least one event, in [`TimelineKind::ALL`] order.
    /// Drives the filter cycle so empty kinds are skipped.
    pub(crate) fn present_kinds(&self) -> Vec<TimelineKind> {
        TimelineKind::ALL
            .iter()
            .copied()
            .filter(|&kind| self.events.iter().any(|e| e.kind == kind))
            .collect()
    }

    /// The events passing the active filter (chronological order). When the
    /// filter is `None` this is every event; otherwise only events of the
    /// filtered kind. Borrowed clones are avoided — returns references.
    pub(crate) fn visible(&self) -> Vec<&TimelineEvent> {
        self.events
            .iter()
            .filter(|e| self.filter.is_none_or(|k| e.kind == k))
            .collect()
    }

    /// Number of events passing the active filter.
    pub(crate) fn visible_len(&self) -> usize {
        match self.filter {
            None => self.events.len(),
            Some(kind) => self.events.iter().filter(|e| e.kind == kind).count(),
        }
    }

    /// The visible event at list index `index` (after the active filter), or
    /// `None` when out of range. Drives the overlay's selectable rows.
    pub(crate) fn visible_get(&self, index: usize) -> Option<&TimelineEvent> {
        self.visible().into_iter().nth(index)
    }

    /// Number of events of `kind` (unfiltered).
    pub(crate) fn count_of(&self, kind: TimelineKind) -> usize {
        self.events.iter().filter(|e| e.kind == kind).count()
    }

    /// Number of failed events (any kind whose status is
    /// [`TimelineStatus::Failed`]).
    pub(crate) fn failed_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| e.status == TimelineStatus::Failed)
            .count()
    }

    /// Number of pending (in-flight) events.
    pub(crate) fn pending_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| e.status == TimelineStatus::Pending)
            .count()
    }

    /// The number of distinct turns the timeline spans (the maximum turn ordinal
    /// seen). 0 for an empty timeline. Events are grouped by this turn ordinal.
    pub(crate) fn turn_count(&self) -> u32 {
        self.events.iter().map(|e| e.turn).max().unwrap_or(0)
    }

    /// The visible list index of the next event strictly after `after` (wrapping
    /// to the first when `after` is the last or `None`). `None` only when nothing
    /// is visible. Drives forward quick-jump navigation over the *filtered* list.
    pub(crate) fn next_index(&self, after: Option<usize>) -> Option<usize> {
        let count = self.visible_len();
        if count == 0 {
            return None;
        }
        match after {
            Some(i) if i + 1 < count => Some(i + 1),
            Some(_) => Some(0),
            None => Some(0),
        }
    }

    /// The visible list index of the previous event strictly before `before`
    /// (wrapping to the last). `None` only when nothing is visible. Drives
    /// backward quick-jump navigation; the overlay walks forward on Enter today,
    /// so this is exercised by the unit suite until a "previous event" verb
    /// lands.
    #[cfg(test)]
    pub(crate) fn prev_index(&self, before: Option<usize>) -> Option<usize> {
        let count = self.visible_len();
        if count == 0 {
            return None;
        }
        match before {
            Some(0) | None => Some(count - 1),
            Some(i) if i <= count => Some(i - 1),
            Some(_) => Some(count - 1),
        }
    }

    /// A compact one-line summary of the timeline for the status line / overlay
    /// header, e.g. `"6 events \u{00b7} 2 turns \u{00b7} 2 tool \u{00b7} 1 error
    /// \u{00b7} 1 failed"`. Empty string when the timeline is empty.
    pub(crate) fn summary(&self) -> String {
        if self.events.is_empty() {
            return String::new();
        }
        let total = self.events.len();
        let total_word = if total == 1 { "event" } else { "events" };
        let mut parts = vec![format!("{total} {total_word}")];
        let turns = self.turn_count();
        if turns > 0 {
            let turn_word = if turns == 1 { "turn" } else { "turns" };
            parts.push(format!("{turns} {turn_word}"));
        }
        for kind in TimelineKind::ALL.iter().copied() {
            let n = self.count_of(kind);
            if n > 0 {
                parts.push(format!("{n} {}", kind.label()));
            }
        }
        let failed = self.failed_count();
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        let pending = self.pending_count();
        if pending > 0 {
            parts.push(format!("{pending} pending"));
        }
        parts.join(" \u{00b7} ")
    }
}

#[cfg(test)]
#[path = "session_timeline_tests.rs"]
mod tests;
