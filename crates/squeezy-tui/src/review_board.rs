//! Live Review Board (§12.8.5): a fan-out orchestration dashboard that groups the
//! session's in-flight and finished subagents/workers into status lanes — a glance
//! summary of "what is running, what is blocked, what got capped, what finished"
//! across a parallel delegation. The board is the spec's "fanout orchestration
//! board with queued, running, reviewing, blocked, and completed lanes" where
//! "capped/rejected workers remain visible".
//!
//! **Derived from the subagent records, not a parallel data source.** As the spec
//! requires ("derive board from `SubagentRecord` plus planned-work records"), the
//! board is projected from the SAME
//! [`crate::subagent_timeline::SubagentTimelineSource`] rows the Subagent Timeline
//! Panel (§12.8.1) already builds from the live subagent-pane records — id, agent,
//! lifecycle status, latest activity, elapsed, tool count, cost. The board does not
//! re-read the pane; the crate root feeds it the same projected slice, so the two
//! views can never disagree about a subagent's state. There is no separate
//! "planned-work" record store in this codebase, so the board honestly derives its
//! lanes only from the real lifecycle the records carry.
//!
//! **Lanes are honest about runtime state — no inferred queueing.** The spec warns
//! twice that "queued can mislead if it is not actual runtime admission" and "do
//! not infer runtime queueing from cap rejection". This codebase's
//! [`crate::subagent_timeline::SubagentTimelineStatus`] carries exactly four real
//! states (running, completed, failed, cap-rejected) — there is no runtime
//! "admitted-but-not-started" (queued) signal and no distinct "reviewing" phase to
//! read, so the board does NOT fabricate a Queued lane from cap rejections.
//! Instead it maps each real status to an honestly-labelled lane:
//! - a running subagent → [`ReviewLane::Running`],
//! - a cap-rejected worker → [`ReviewLane::Capped`] (kept visible, never relabelled
//!   "queued"),
//! - a failed subagent → [`ReviewLane::Blocked`],
//! - a finished subagent → [`ReviewLane::Completed`].
//!
//! Capped and blocked lanes are attention lanes (the spec's "capped/rejected
//! workers remain visible"); a worker never silently drops off the board.
//!
//! **Stable-id navigation across lanes.** The board flattens its lanes into a
//! single lane-ordered visiting sequence and navigates it by the subagent's stable
//! `id` (never a `Vec` index), so a record that vanishes (pruned/cleared) or a lane
//! that empties out can never repoint the cursor at the wrong worker — the cursor
//! heals to the nearest surviving worker. This mirrors the id-addressing the
//! compare view (§12.8.3) and the timeline panel (§12.8.1) use.
//!
//! **Zero idle cost, incremental rebuild.** Like the Subagent Timeline, the board
//! carries a `fingerprint` over its source rows and rebuilds only when that moves
//! (a real subagent lifecycle event). An idle session that never opens the board
//! pays one `u64` comparison per refresh and rebuilds nothing; a session that never
//! opens the board at all pays nothing, because the crate root refreshes it lazily
//! only while the overlay is open.
//!
//! **Pure state/model, no `TuiApp`.** Like its peer leaf modules
//! (`subagent_timeline`, `subagent_compare`, `pinned_compare`, `interaction`) this
//! file holds only the lane model, the classification predicate, navigation, and
//! the summary — nothing about geometry, rendering, or input. The crate root owns
//! the open flag, the keybinding, the per-frame render call, and the
//! id→conversation jump. Keeping the model here lets the lane/navigation math be
//! unit-tested without a terminal.

use crate::subagent_timeline::{SubagentTimelineSource, SubagentTimelineStatus};

/// One status lane on the Live Review Board (§12.8.5). The order of the variants is
/// the board's left-to-right (wide) / top-to-bottom (narrow tabs) lane order and
/// the order the flattened navigation sequence visits — attention-worthy lanes
/// (blocked, capped) sit between the live (running) and the settled (completed)
/// lanes so a glance reads "in flight → needs a look → done".
///
/// Deliberately NOT a "Queued" lane: this codebase has no runtime
/// admitted-but-not-started signal, and the spec forbids inferring queueing from
/// cap rejection. Every lane here maps to a real, observed lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ReviewLane {
    /// Workers still running (no terminal event yet) — the live fan-out.
    Running,
    /// Workers that ended in a failure — blocked, needs a look (attention).
    Blocked,
    /// Workers refused before they ran because the concurrency cap was hit — kept
    /// visible per the spec, never relabelled "queued" (attention).
    Capped,
    /// Workers that finished normally with a result — the settled lane.
    Completed,
}

impl ReviewLane {
    /// Every lane, in board (left-to-right / navigation) order. Exhaustive on
    /// purpose: a new lane must be added here or it never appears on the board.
    pub(crate) const ALL: &'static [ReviewLane] = &[
        ReviewLane::Running,
        ReviewLane::Blocked,
        ReviewLane::Capped,
        ReviewLane::Completed,
    ];

    /// The lane a subagent's lifecycle status belongs in. Total over the real
    /// status set, and honest: a cap rejection maps to [`ReviewLane::Capped`]
    /// (kept visible), never to a fabricated "queued" lane.
    pub(crate) fn classify(status: SubagentTimelineStatus) -> ReviewLane {
        match status {
            SubagentTimelineStatus::Running => ReviewLane::Running,
            SubagentTimelineStatus::Failed => ReviewLane::Blocked,
            SubagentTimelineStatus::Rejected => ReviewLane::Capped,
            SubagentTimelineStatus::Completed => ReviewLane::Completed,
        }
    }

    /// Short, screen-reader-friendly lane title. ASCII only (no glyphs) so the
    /// board carries meaning without relying on color.
    pub(crate) fn title(self) -> &'static str {
        match self {
            ReviewLane::Running => "Running",
            ReviewLane::Blocked => "Blocked",
            ReviewLane::Capped => "Capped",
            ReviewLane::Completed => "Completed",
        }
    }

    /// Whether this lane carries workers that want a look — blocked (failed) and
    /// capped (cap-rejected) are the attention lanes; running and completed are
    /// calm. Drives the board's attention emphasis and summary count.
    pub(crate) fn is_attention(self) -> bool {
        matches!(self, ReviewLane::Blocked | ReviewLane::Capped)
    }

    /// A one-line gloss for the focused lane, naming the lifecycle the label
    /// stands for and the remediation it implies. `Blocked` and `Capped` both
    /// read as "failure" at a glance, but a capped worker only needs a retry
    /// once capacity frees up while a blocked one ran and failed — the gloss
    /// makes that distinction explicit when a lane is focused.
    pub(crate) fn gloss(self) -> &'static str {
        match self {
            ReviewLane::Running => "Running — in flight, no terminal event yet",
            ReviewLane::Blocked => "Blocked — ran and failed",
            ReviewLane::Capped => "Capped — refused before start (concurrency cap)",
            ReviewLane::Completed => "Completed — finished with a result",
        }
    }
}

/// One worker card on the board: a flattened, board-ordered projection of a source
/// row. Carries the subagent's stable `id` (for id navigation + the
/// id→conversation jump), its `lane`, its `agent`/role label, its 1-based
/// `ordinal` (parallel-fanout disambiguation, matching the timeline), its short
/// `latest` activity label, and the structured `elapsed_secs` / `tool_count` /
/// `cost_micros` metrics (formatted at the edge so an unknown metric reads `"-"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewCard {
    pub(crate) id: u64,
    pub(crate) lane: ReviewLane,
    pub(crate) ordinal: u32,
    pub(crate) agent: String,
    pub(crate) latest: String,
    pub(crate) elapsed_secs: Option<u64>,
    pub(crate) tool_count: u64,
    pub(crate) cost_micros: Option<u64>,
}

impl ReviewCard {
    /// The elapsed time as a compact `m:ss` clock (rolling over to `h:mm:ss` once
    /// past an hour), or `"-"` when the source carried no start time (an honest
    /// "no timing" rendering for a cap-rejected worker).
    pub(crate) fn elapsed_clock(&self) -> String {
        match self.elapsed_secs {
            Some(secs) if secs >= 3600 => {
                format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
            }
            Some(secs) => format!("{}:{:02}", secs / 60, secs % 60),
            None => "-".to_string(),
        }
    }

    /// The reported cost as `$x.xxxxxx`, or `"-"` when the child reported no cost.
    /// Formats the structured `cost_micros` only here so the board never invents a
    /// number — an unknown cost reads honestly as `"-"`.
    pub(crate) fn cost_label(&self) -> String {
        match self.cost_micros {
            Some(micros) => format!("${:.6}", micros as f64 / 1_000_000.0),
            None => "-".to_string(),
        }
    }
}

/// The computed Live Review Board (§12.8.5) over the session's subagent records.
///
/// `cards` is the flattened, board-ordered (lane order, record order within a lane)
/// list of worker cards — the same order the cursor visits. `fingerprint` is the
/// staleness tag; `built` distinguishes "empty set built" from "never built" so a
/// genuinely empty session is not re-walked every refresh.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReviewBoard {
    cards: Vec<ReviewCard>,
    fingerprint: u64,
    built: bool,
}

impl ReviewBoard {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Fold a staleness fingerprint over the sources — the SAME fields the subagent
    /// timeline folds (id, status, elapsed, tool count, cost, agent, latest), so a
    /// new worker, a status flip, a tool tick, a cost update, or a fresh activity
    /// line all move the value. Pure and standalone so the caller can compute it
    /// cheaply each refresh and compare before deciding to recompute.
    pub(crate) fn fingerprint_of<'a>(
        sources: impl IntoIterator<Item = &'a SubagentTimelineSource>,
    ) -> u64 {
        use std::hash::{Hash, Hasher};
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

    /// Recompute the board from `sources` **only if** `fingerprint` differs from
    /// the one captured at the last rebuild (or this is the first build). Returns
    /// `true` when a recompute actually ran, `false` when the cached board was
    /// already current (the zero-idle-cost fast path).
    ///
    /// The cards are laid out lane-major: for each lane in [`ReviewLane::ALL`]
    /// order, every source classified into that lane in its source (record) order.
    /// A source's 1-based `ordinal` is its position in the ORIGINAL source slice
    /// (so it matches the Subagent Timeline Panel's `agent #ordinal`), not its
    /// position on the board.
    pub(crate) fn rebuild_if_stale(
        &mut self,
        fingerprint: u64,
        sources: &[SubagentTimelineSource],
    ) -> bool {
        if self.built && fingerprint == self.fingerprint {
            return false;
        }
        self.cards.clear();
        self.cards.reserve(sources.len());
        for lane in ReviewLane::ALL.iter().copied() {
            for (index, source) in sources.iter().enumerate() {
                if ReviewLane::classify(source.status) != lane {
                    continue;
                }
                self.cards.push(ReviewCard {
                    id: source.id,
                    lane,
                    ordinal: index as u32 + 1,
                    agent: source.agent.clone(),
                    latest: crate::subagent_timeline::clean_label(&source.latest, source.status),
                    elapsed_secs: source.elapsed_secs,
                    tool_count: source.tool_count,
                    cost_micros: source.cost_micros,
                });
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

    /// Every card in board order (lane-major). The production paths navigate by
    /// stable id ([`Self::index_of`] / [`Self::card_at`]) and render per-lane
    /// ([`Self::cards_in`]); this whole-slice accessor is read by the unit suite to
    /// assert the lane-major layout/order.
    #[cfg(test)]
    pub(crate) fn cards(&self) -> &[ReviewCard] {
        &self.cards
    }

    /// Total number of worker cards across every lane. The production paths use
    /// [`Self::is_empty`] and the per-lane [`Self::count_in`]; this total is read by
    /// the unit suite and the integration tests.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.cards.len()
    }

    /// Whether the board has any workers at all.
    pub(crate) fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    /// The card at flattened board index `index`, or `None` when out of range.
    pub(crate) fn card_at(&self, index: usize) -> Option<&ReviewCard> {
        self.cards.get(index)
    }

    /// The cards in one lane, in record order. Returns references — no clones.
    /// Drives the per-lane column / tab rendering.
    pub(crate) fn cards_in(&self, lane: ReviewLane) -> Vec<&ReviewCard> {
        self.cards.iter().filter(|c| c.lane == lane).collect()
    }

    /// Number of workers in one lane.
    pub(crate) fn count_in(&self, lane: ReviewLane) -> usize {
        self.cards.iter().filter(|c| c.lane == lane).count()
    }

    /// The lanes that actually have at least one worker, in [`ReviewLane::ALL`]
    /// order. Lets the renderer skip painting an empty lane's body while still
    /// counting it in the summary.
    pub(crate) fn present_lanes(&self) -> Vec<ReviewLane> {
        ReviewLane::ALL
            .iter()
            .copied()
            .filter(|&lane| self.count_in(lane) > 0)
            .collect()
    }

    /// Number of workers in an attention lane (blocked or capped) — the count the
    /// board surfaces so a glance shows whether anything needs a look.
    pub(crate) fn attention_count(&self) -> usize {
        self.cards.iter().filter(|c| c.lane.is_attention()).count()
    }

    /// The flattened board index of the card with stable `id`, or `None` when no
    /// such worker is on the board (it vanished). Used to re-find the cursor's
    /// worker across a rebuild so navigation is by id, never by raw index.
    pub(crate) fn index_of(&self, id: u64) -> Option<usize> {
        self.cards.iter().position(|c| c.id == id)
    }

    /// The flattened board index of the next card strictly after `after` (wrapping
    /// to the first when `after` is the last or `None`). `None` only when the board
    /// is empty. Drives forward navigation across every lane.
    pub(crate) fn next_index(&self, after: Option<usize>) -> Option<usize> {
        let count = self.cards.len();
        if count == 0 {
            return None;
        }
        match after {
            Some(i) if i + 1 < count => Some(i + 1),
            _ => Some(0),
        }
    }

    /// The flattened board index of the previous card strictly before `before`
    /// (wrapping to the last). `None` only when the board is empty. Drives backward
    /// navigation across every lane.
    pub(crate) fn prev_index(&self, before: Option<usize>) -> Option<usize> {
        let count = self.cards.len();
        if count == 0 {
            return None;
        }
        match before {
            Some(0) | None => Some(count - 1),
            Some(i) => Some(i - 1),
        }
    }

    /// A compact one-line summary for the board header / status line, e.g.
    /// `"5 workers \u{00b7} 2 Running \u{00b7} 1 Blocked \u{00b7} 1 Capped \u{00b7}
    /// 1 Completed \u{00b7} 2 attention"`. Empty string when the board is empty.
    pub(crate) fn summary(&self) -> String {
        if self.cards.is_empty() {
            return String::new();
        }
        let total = self.cards.len();
        let worker_word = if total == 1 { "worker" } else { "workers" };
        let mut parts = vec![format!("{total} {worker_word}")];
        for lane in ReviewLane::ALL.iter().copied() {
            let n = self.count_in(lane);
            if n > 0 {
                parts.push(format!("{n} {}", lane.title()));
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
#[path = "review_board_tests.rs"]
mod tests;
