//! Subagent Timeline Panel (§12.8.1): a persistent, navigable panel that lists
//! the running and completed subagents/tasks of the session on a timeline — each
//! row carries the subagent's id/name, role, lifecycle status, latest activity,
//! elapsed time, tool count, and (where the child reported it) cost, plus an
//! attention flag for the rows that need a look (a failure or a cap rejection).
//!
//! The panel is sourced from the **existing** subagent tracking the TUI already
//! renders — the `SubagentPaneState` records `lib.rs` maintains from the live
//! subagent lifecycle events (start / activity / tool result / completed /
//! failed / rejected). `lib.rs` projects each record into a
//! [`SubagentTimelineSource`] (a small structured snapshot — id, agent, status,
//! latest line, elapsed seconds, tool count, optional cost micros), and this
//! module turns that slice into an ordered, filterable timeline and answers
//! list/navigation queries. The module is deliberately **pure**: it owns the
//! row/navigation/filter bookkeeping and nothing about geometry, rendering, or
//! input, so the timeline math is unit-testable without a terminal — exactly the
//! shape of the Session Timeline (§12.2.6).
//!
//! **Structured events, not formatted strings.** As the spec warns, accurate
//! cost depends on child metrics and retention must be bounded. The source rows
//! carry *structured numbers* (`elapsed_secs`, `tool_count`, `cost_micros`), and
//! this module formats them at the edge ([`SubagentTimelineEntry::elapsed_clock`],
//! [`SubagentTimelineEntry::cost_label`]) — a subagent whose child reported no
//! cost renders an honest `"-"` rather than an invented number. Retention is the
//! caller's: it feeds the already-pruned record slice, so the timeline never
//! grows past the pane's bound.
//!
//! **Synthetic / cap-rejected records are first-class.** A subagent refused
//! before it ran (the concurrency cap was hit) has no lease id and no metrics; it
//! still appears as a [`SubagentTimelineStatus::Rejected`] row so the cap is
//! visible on the timeline. Its elapsed/cost columns read `"-"` honestly.
//!
//! **Zero idle cost, incremental rebuild.** Like the Session Timeline, the model
//! carries a `fingerprint` folded over every source `(id, status, elapsed,
//! tool_count, cost, latest, agent)`. The caller feeds the same fingerprint each
//! refresh via [`SubagentTimeline::rebuild_if_stale`]; when it matches the stored
//! one the call returns immediately and touches nothing. The timeline is only
//! re-walked when a subagent record actually changed (a lifecycle event, an
//! activity line, a tool result, a completion) — exactly the events that move the
//! fingerprint. An idle session pays one cheap `u64` comparison per refresh and
//! rebuilds nothing.

use std::hash::{Hash, Hasher};

/// The lifecycle status of a subagent on the timeline — the same small, fixed set
/// the subagent pane tracks (`SubagentLifecycle`), re-declared here so the pure
/// timeline module does not depend on `lib.rs`. `lib.rs` maps its private
/// lifecycle enum onto this one when it builds the source rows. Ordered so
/// [`SubagentTimelineStatus::ALL`] reads attention-first (the rows a user most
/// needs to see — failures and caps — sort to the front of the filter cycle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SubagentTimelineStatus {
    /// The subagent is still running (no terminal event yet).
    Running,
    /// The subagent finished normally with a summary.
    Completed,
    /// The subagent ended in a failure.
    Failed,
    /// The subagent was refused before it ran (the concurrency cap was hit). It
    /// carries no lease id and no metrics — a synthetic, attention-worthy row.
    Rejected,
}

impl SubagentTimelineStatus {
    /// Every status, in timeline filter-cycle order. Exhaustive on purpose: a new
    /// variant must be added here or it never appears in the summary / filter
    /// cycle.
    pub(crate) const ALL: &'static [SubagentTimelineStatus] = &[
        SubagentTimelineStatus::Running,
        SubagentTimelineStatus::Completed,
        SubagentTimelineStatus::Failed,
        SubagentTimelineStatus::Rejected,
    ];

    /// Short, screen-reader-friendly label for the status tag. ASCII only (no
    /// glyphs) so the timeline carries meaning without relying on color or a
    /// private-use codepoint.
    pub(crate) fn label(self) -> &'static str {
        match self {
            SubagentTimelineStatus::Running => "running",
            SubagentTimelineStatus::Completed => "done",
            SubagentTimelineStatus::Failed => "failed",
            // "rejected" (matching the `Rejected` variant) is honest about what
            // happened to the agent — it never ran — and reserves "cap" for the
            // compare view's two-slot column bound, which is an unrelated concept.
            SubagentTimelineStatus::Rejected => "rejected",
        }
    }

    /// Whether this status wants the user's attention — a failed or cap-rejected
    /// subagent is flagged so the panel can mark it (the spec's "attention
    /// state"). A running or completed subagent is calm.
    pub(crate) fn is_attention(self) -> bool {
        matches!(
            self,
            SubagentTimelineStatus::Failed | SubagentTimelineStatus::Rejected
        )
    }

    /// Whether this status is terminal (the subagent will produce no further
    /// events). Running is the only non-terminal state.
    pub(crate) fn is_running(self) -> bool {
        matches!(self, SubagentTimelineStatus::Running)
    }
}

/// One subagent record as the caller projects it in. `id` is the subagent's
/// stable session-local id (a real lease id, or a synthetic high id for a
/// cap-rejection); `agent` is its role/name label; `status` is its lifecycle
/// state; `latest` is its most recent activity line (already bounded and
/// secret-free by the caller); `elapsed_secs` is how long it has run (wall time
/// since it started, or its total run time once finished) — `None` for a record
/// with no usable start time (e.g. a cap rejection); `tool_count` is the number
/// of tool calls the child reported; `cost_micros` is its reported spend in USD
/// micros, `None` when the child reported no cost (the spec's "accurate cost
/// depends on child metrics" case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentTimelineSource {
    pub(crate) id: u64,
    pub(crate) agent: String,
    pub(crate) status: SubagentTimelineStatus,
    /// The raw, caller-supplied latest-activity line. Cleaned + bounded +
    /// deterministically fallen-back here.
    pub(crate) latest: String,
    pub(crate) elapsed_secs: Option<u64>,
    pub(crate) tool_count: u64,
    pub(crate) cost_micros: Option<u64>,
}

/// One row on the Subagent Timeline (§12.8.1). `id` is the subagent's stable id;
/// `ordinal` is its 1-based display position (parallel-fanout disambiguation);
/// `agent` is its role/name; `status` is its lifecycle state; `latest` is the
/// short, bounded, deterministic activity label; `elapsed_secs`, `tool_count`,
/// and `cost_micros` are the structured metrics (formatted at the edge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentTimelineEntry {
    pub(crate) id: u64,
    pub(crate) ordinal: u32,
    pub(crate) agent: String,
    pub(crate) status: SubagentTimelineStatus,
    pub(crate) latest: String,
    pub(crate) elapsed_secs: Option<u64>,
    pub(crate) tool_count: u64,
    pub(crate) cost_micros: Option<u64>,
}

impl SubagentTimelineEntry {
    /// The elapsed time rendered as a compact `m:ss` clock (rolling over to
    /// `h:mm:ss` once past an hour), or `"-"` when the source carried no start
    /// time (an honest "no timing" rendering for a cap-rejected record). Pure so
    /// the timing column is unit-testable without a terminal.
    pub(crate) fn elapsed_clock(&self) -> String {
        match self.elapsed_secs {
            Some(secs) if secs >= 3600 => {
                format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
            }
            Some(secs) => {
                let minutes = secs / 60;
                let seconds = secs % 60;
                format!("{minutes}:{seconds:02}")
            }
            None => "-".to_string(),
        }
    }

    /// The reported cost rendered as `$x.xxxxxx`, or `"-"` when the child
    /// reported no cost. Deliberately formats the structured `cost_micros` only
    /// here (never carried as a pre-formatted string), so the panel never invents
    /// a number — an unknown cost reads honestly as `"-"`.
    pub(crate) fn cost_label(&self) -> String {
        match self.cost_micros {
            Some(micros) => format!("${:.6}", micros as f64 / 1_000_000.0),
            None => "-".to_string(),
        }
    }

    /// Whether this row wants attention (its status is failed or cap-rejected).
    pub(crate) fn is_attention(&self) -> bool {
        self.status.is_attention()
    }
}

/// Largest number of characters retained in a row's `latest` label. One short
/// line: long enough to disambiguate, short enough that a row never wraps the
/// panel.
const LABEL_CAP: usize = 60;

/// Clean a caller-supplied raw activity line into a bounded one-line label,
/// falling back to `"(status)"` when the source has no usable text.
///
/// Deterministic and honest: takes the first non-empty line, collapses interior
/// whitespace runs to single spaces, trims, and caps to [`LABEL_CAP`] chars (on a
/// char boundary, appending an ellipsis when cut). A blank source yields
/// `"(status)"` rather than inventing text.
pub(crate) fn clean_label(raw: &str, status: SubagentTimelineStatus) -> String {
    let first_line = raw.lines().map(str::trim).find(|line| !line.is_empty());
    let Some(line) = first_line else {
        return format!("({})", status.label());
    };
    let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return format!("({})", status.label());
    }
    if collapsed.chars().count() <= LABEL_CAP {
        return collapsed;
    }
    let prefix: String = collapsed.chars().take(LABEL_CAP).collect();
    format!("{prefix}\u{2026}")
}

/// Build the timeline row for one source at display position `ordinal` (1-based).
/// Pure and standalone so it is the unit-testable heart of the feature: the
/// activity label is cleaned/bounded and the id/status/metrics pass through.
pub(crate) fn entry_for_source(
    source: &SubagentTimelineSource,
    ordinal: u32,
) -> SubagentTimelineEntry {
    SubagentTimelineEntry {
        id: source.id,
        ordinal,
        agent: source.agent.clone(),
        status: source.status,
        latest: clean_label(&source.latest, source.status),
        elapsed_secs: source.elapsed_secs,
        tool_count: source.tool_count,
        cost_micros: source.cost_micros,
    }
}

/// The computed Subagent Timeline over the session's subagent records (§12.8.1).
///
/// `entries` is the ordered list of rows (record order — the order the subagents
/// started, matching the pane). `filter` is the active status filter (`None` =
/// show all); the rendered list is the rows passing it. `fingerprint` is the
/// staleness tag described in the module docs; `built` distinguishes "empty set
/// built" from "never built" so a genuinely empty session is not re-walked every
/// refresh.
#[derive(Debug, Clone, Default)]
pub(crate) struct SubagentTimeline {
    entries: Vec<SubagentTimelineEntry>,
    filter: Option<SubagentTimelineStatus>,
    fingerprint: u64,
    built: bool,
}

impl SubagentTimeline {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Fold a staleness fingerprint over the sources. Order- and
    /// content-sensitive: id, status, elapsed, tool count, cost, agent, and the
    /// latest line all participate, so a new subagent, a status flip, a tool
    /// tick, a cost update, or a fresh activity line all move the value. Pure and
    /// standalone so the caller can compute it cheaply each refresh and compare
    /// before deciding to recompute. The active filter is *not* folded in —
    /// filtering is a cheap view operation that never needs a rebuild.
    pub(crate) fn fingerprint_of<'a>(
        sources: impl IntoIterator<Item = &'a SubagentTimelineSource>,
    ) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for source in sources {
            source.id.hash(&mut hasher);
            source.status.hash(&mut hasher);
            source.elapsed_secs.hash(&mut hasher);
            source.tool_count.hash(&mut hasher);
            source.cost_micros.hash(&mut hasher);
            source.agent.hash(&mut hasher);
            source.latest.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Recompute the timeline from `sources` **only if** `fingerprint` differs
    /// from the one captured at the last rebuild (or this is the first build).
    /// Returns `true` when a recompute actually ran, `false` when the cached
    /// timeline was already current (the zero-idle-cost fast path).
    ///
    /// The caller computes `fingerprint` via [`Self::fingerprint_of`] over the
    /// same slice. Dropped records fall out implicitly: the rebuild starts from
    /// an empty set and only the sources present in `sources` survive. The active
    /// filter is preserved across a rebuild (it is a view setting, not data).
    pub(crate) fn rebuild_if_stale(
        &mut self,
        fingerprint: u64,
        sources: &[SubagentTimelineSource],
    ) -> bool {
        if self.built && fingerprint == self.fingerprint {
            return false;
        }
        self.entries.clear();
        self.entries.reserve(sources.len());
        for (index, source) in sources.iter().enumerate() {
            self.entries
                .push(entry_for_source(source, index as u32 + 1));
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

    /// Every row in record order, regardless of the active filter. The production
    /// panel paints the *visible* (filtered) slice via [`Self::visible`]; the
    /// unfiltered list is read by the unit suite to assert order/metrics.
    #[cfg(test)]
    pub(crate) fn entries(&self) -> &[SubagentTimelineEntry] {
        &self.entries
    }

    /// Total number of rows (unfiltered).
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the timeline has any rows (unfiltered).
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The active status filter, or `None` when every row is shown.
    pub(crate) fn filter(&self) -> Option<SubagentTimelineStatus> {
        self.filter
    }

    /// Cycle the active status filter forward: `None` (show all) → each
    /// **non-empty** status in [`SubagentTimelineStatus::ALL`] order → back to
    /// `None`. Only statuses that actually have a row are offered, so the cycle
    /// never lands on a filter that would show nothing. A no-op (stays `None`)
    /// when the timeline is empty.
    pub(crate) fn cycle_filter(&mut self) {
        let present = self.present_statuses();
        if present.is_empty() {
            self.filter = None;
            return;
        }
        self.filter = match self.filter {
            None => present.first().copied(),
            Some(current) => {
                let pos = present.iter().position(|&s| s == current);
                match pos {
                    Some(i) if i + 1 < present.len() => present.get(i + 1).copied(),
                    Some(_) => None,
                    // Current filter is no longer present (its rows dropped):
                    // fall back to "show all" rather than a stale status.
                    None => None,
                }
            }
        };
    }

    /// The statuses that have at least one row, in [`SubagentTimelineStatus::ALL`]
    /// order. Drives the filter cycle so empty statuses are skipped.
    pub(crate) fn present_statuses(&self) -> Vec<SubagentTimelineStatus> {
        SubagentTimelineStatus::ALL
            .iter()
            .copied()
            .filter(|&status| self.entries.iter().any(|e| e.status == status))
            .collect()
    }

    /// The rows passing the active filter (record order). When the filter is
    /// `None` this is every row; otherwise only rows of the filtered status.
    /// Returns references — no clones.
    pub(crate) fn visible(&self) -> Vec<&SubagentTimelineEntry> {
        self.entries
            .iter()
            .filter(|e| self.filter.is_none_or(|s| e.status == s))
            .collect()
    }

    /// Number of rows passing the active filter.
    pub(crate) fn visible_len(&self) -> usize {
        match self.filter {
            None => self.entries.len(),
            Some(status) => self.entries.iter().filter(|e| e.status == status).count(),
        }
    }

    /// The visible row at list index `index` (after the active filter), or `None`
    /// when out of range. Drives the panel's selectable rows.
    pub(crate) fn visible_get(&self, index: usize) -> Option<&SubagentTimelineEntry> {
        self.visible().into_iter().nth(index)
    }

    /// Number of rows of `status` (unfiltered).
    pub(crate) fn count_of(&self, status: SubagentTimelineStatus) -> usize {
        self.entries.iter().filter(|e| e.status == status).count()
    }

    /// Number of rows that want attention (failed or cap-rejected), unfiltered.
    pub(crate) fn attention_count(&self) -> usize {
        self.entries.iter().filter(|e| e.is_attention()).count()
    }

    /// Number of still-running rows (unfiltered).
    pub(crate) fn running_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.status.is_running())
            .count()
    }

    /// The visible list index of the next row strictly after `after` (wrapping to
    /// the first when `after` is the last or `None`). `None` only when nothing is
    /// visible. Drives forward navigation over the *filtered* list.
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

    /// The visible list index of the previous row strictly before `before`
    /// (wrapping to the last). `None` only when nothing is visible. Drives
    /// backward navigation over the *filtered* list; the panel walks forward on
    /// Enter today, so this is exercised by the unit suite until a "previous
    /// subagent" verb lands.
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

    /// A compact one-line summary for the status line / panel header, e.g.
    /// `"3 subagents \u{00b7} 1 running \u{00b7} 1 done \u{00b7} 1 failed \u{00b7}
    /// 1 attention"`. Empty string when the timeline is empty.
    pub(crate) fn summary(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let total = self.entries.len();
        let total_word = if total == 1 { "subagent" } else { "subagents" };
        let mut parts = vec![format!("{total} {total_word}")];
        for status in SubagentTimelineStatus::ALL.iter().copied() {
            let n = self.count_of(status);
            if n > 0 {
                parts.push(format!("{n} {}", status.label()));
            }
        }
        let attention = self.attention_count();
        if attention > 0 {
            parts.push(format!("{attention} attention"));
        }
        parts.join(" \u{00b7} ")
    }
}

#[cfg(test)]
#[path = "subagent_timeline_tests.rs"]
mod tests;
