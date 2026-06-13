//! Collapsible Reasoning/Tool Lanes (§12.2.2): split a dense transcript entry
//! into foldable *lanes* — assistant text, reasoning summary, tool input, tool
//! output, system notice, approval, error, and plan — so the transcript can be
//! read "at a higher altitude". Each lane can be collapsed (header only) or
//! expanded (full body); collapsing whole lanes keeps the main view concise
//! while the detail is one keystroke / click away.
//!
//! **Reuses the existing fold/collapse model.** A collapsed lane is exactly the
//! crate's existing "folded" idea applied to one *lane of one entry* instead of
//! a whole entry: the same `Collapsed`/`Expanded` two-state switch the row model
//! ([`crate::transcript_surface::FoldState`]) and the Ctrl+T overlay
//! ([`crate::OverlayDetail`]) already speak. This module owns only the per-lane
//! *bookkeeping* — the lane taxonomy, the `(entry_id, lane_id)`-keyed fold
//! store, and the navigation/summary queries — and nothing about geometry,
//! rendering, or input. `lib.rs` decomposes the focused transcript entry into a
//! [`LaneEntry`] slice (reusing the same `TranscriptEntryKind` / `ToolStatus` /
//! `entry_is_error` predicates the renderer and jump-nav use) and feeds it in;
//! this module turns those facts into an ordered, foldable lane list and answers
//! toggle/navigation queries. That keeps the lane math testable without a
//! terminal.
//!
//! **Fold state keyed by `(entry_id, lane_id)`, persisted across redraws.** The
//! spec's contract is "store fold state by `(entry_id, lane_id)`": a
//! [`LaneFoldStore`] is a set of the `(entry_id, lane_id)` pairs the user has
//! collapsed. The store lives in `TuiApp`, so a toggle survives every redraw,
//! resize, scroll, and stream tick — the collapse state is app-owned UI state,
//! never recomputed from terminal cells. Keying by the stable
//! `TranscriptEntry::id` (never a width-/fold-dependent row coordinate, like the
//! transcript index, jump-mark stack, and turn outline) means a lane stays
//! collapsed across reflow: an id survives resize, streaming, and coalescing, so
//! a collapse set captured before a reflow still resolves to the right lanes
//! afterwards.
//!
//! **Hiding failures is the spec's named risk; errored lanes keep visible
//! headers.** A lane is never *removed* when collapsed — only its body is
//! hidden, the header (and its line-count / `error` tag) always paints. So a
//! collapsed error lane still shouts that something failed; the user can see the
//! failure exists and expand it. This is the spec's "errored lanes keep visible
//! headers" mitigation, enforced in the model: [`Lane::is_error`] lanes report
//! [`Lane::always_visible`] regardless of collapse state.
//!
//! **Zero idle cost.** The lane panel is built lazily — only while the lane-fold
//! overlay is open — and the build carries a `fingerprint` folded over the
//! focused entry's `(id, revision)` plus the collapse store generation. The
//! caller feeds the same fingerprint each refresh via
//! [`LanePanel::rebuild_if_stale`]; when it matches the stored one the call
//! returns immediately and touches nothing. A closed overlay builds nothing at
//! all, and an open-but-idle overlay pays one `u64` comparison per refresh.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};

/// The coarse *lane* a slice of a transcript entry maps to — the spec's fixed
/// lane taxonomy (assistant text, reasoning summary, tool input, tool output,
/// system notice, approval, error, plan). A small, fixed set so a lane has a
/// stable id to key fold state on. Ordered so [`LaneId::ALL`] reads top-to-bottom
/// the way a turn flows (the model's reasoning, then its answer, then a tool's
/// input/output, then any system/approval/error/plan breadcrumb).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum LaneId {
    /// The model's finalized reasoning summary.
    Reasoning,
    /// Assistant answer prose.
    AssistantText,
    /// A tool call's input (command / arguments).
    ToolInput,
    /// A tool call's output (stdout/stderr/result body).
    ToolOutput,
    /// An operational system notice (a log/subagent breadcrumb).
    SystemNotice,
    /// An approval prompt / decision.
    Approval,
    /// A failure surface: a failed tool, a failure log, or an error line.
    Error,
    /// A plan card / plan step text.
    Plan,
}

impl LaneId {
    /// Every lane id, in display order. Exhaustive on purpose: a new variant must
    /// be added here or it never appears in the panel / summary.
    pub(crate) const ALL: &'static [LaneId] = &[
        LaneId::Reasoning,
        LaneId::AssistantText,
        LaneId::ToolInput,
        LaneId::ToolOutput,
        LaneId::SystemNotice,
        LaneId::Approval,
        LaneId::Error,
        LaneId::Plan,
    ];

    /// Short, screen-reader-friendly label for the lane's header. ASCII only (no
    /// glyphs) so the lane carries meaning without relying on color or a
    /// private-use codepoint.
    pub(crate) fn label(self) -> &'static str {
        match self {
            LaneId::Reasoning => "reasoning",
            LaneId::AssistantText => "assistant text",
            LaneId::ToolInput => "tool input",
            LaneId::ToolOutput => "tool output",
            LaneId::SystemNotice => "system notice",
            LaneId::Approval => "approval",
            LaneId::Error => "error",
            LaneId::Plan => "plan",
        }
    }
}

/// One lane as the caller feeds it in, before fold state is applied. `id` is the
/// lane taxonomy slot; `line_count` is how many body rows the lane would paint
/// when expanded (so the header can read "tool output (12 lines)"); `is_error`
/// flags a failure lane (kept always-visible); `preview` is the lane's own first
/// content line (already bounded and secret-free), or empty for a body-less lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaneEntry {
    pub(crate) id: LaneId,
    pub(crate) line_count: usize,
    pub(crate) is_error: bool,
    pub(crate) preview: String,
}

/// The stable fold key for one lane of one entry: `(entry_id, lane_id)`. This is
/// exactly the spec's "store fold state by `(entry_id, lane_id)`" — keyed by the
/// stable `TranscriptEntry::id`, never a row coordinate, so a collapse survives
/// reflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct LaneKey {
    pub(crate) entry_id: u64,
    pub(crate) lane_id: LaneId,
}

impl LaneKey {
    pub(crate) fn new(entry_id: u64, lane_id: LaneId) -> Self {
        Self { entry_id, lane_id }
    }
}

/// One built lane of the focused entry: its [`LaneId`], the resolved
/// collapsed/expanded state (read from the [`LaneFoldStore`]), the body line
/// count, the error flag, and the short deterministic preview line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lane {
    pub(crate) key: LaneKey,
    pub(crate) collapsed: bool,
    pub(crate) line_count: usize,
    pub(crate) is_error: bool,
    pub(crate) preview: String,
}

impl Lane {
    /// The lane's taxonomy slot.
    pub(crate) fn id(&self) -> LaneId {
        self.key.lane_id
    }

    /// Whether the lane's *header* always paints, regardless of collapse state.
    /// True for every lane (a collapsed lane hides only its body, never its
    /// header), but called out as a method because it is load-bearing for the
    /// spec's "errored lanes keep visible headers" risk mitigation: an error lane
    /// must never be hidden by a collapse, and this is where that invariant is
    /// asserted.
    pub(crate) fn always_visible(&self) -> bool {
        // A collapsed lane shows its header; an error lane additionally must
        // always show it — both reduce to "the header always paints".
        true
    }

    /// Whether the lane's *body* paints this frame: an expanded lane shows its
    /// body; a collapsed lane hides it. An error lane is *never* collapsed away
    /// silently — but the user may still collapse its body to scan headers, so
    /// the body visibility honors the fold state while [`Lane::always_visible`]
    /// guarantees the header (with its `error` tag) stays.
    pub(crate) fn body_visible(&self) -> bool {
        !self.collapsed
    }
}

/// Largest number of characters retained in a lane `preview`. One short line:
/// long enough to disambiguate, short enough that a lane header never wraps.
const PREVIEW_CAP: usize = 56;

/// Clean a caller-supplied raw preview into a bounded one-line label. Returns an
/// empty string for a body-less / blank source (the header then reads with just
/// its label + line count). Deterministic and honest: takes the first non-empty
/// line, collapses interior whitespace runs to single spaces, trims, and caps to
/// [`PREVIEW_CAP`] chars on a char boundary (appending an ellipsis when cut).
pub(crate) fn clean_preview(raw: &str) -> String {
    let first_line = raw.lines().map(str::trim).find(|line| !line.is_empty());
    let Some(line) = first_line else {
        return String::new();
    };
    let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return String::new();
    }
    if collapsed.chars().count() <= PREVIEW_CAP {
        return collapsed;
    }
    let prefix: String = collapsed.chars().take(PREVIEW_CAP).collect();
    format!("{prefix}\u{2026}")
}

/// The set of collapsed `(entry_id, lane_id)` lanes, persisted in `TuiApp` across
/// redraws (§12.2.2). A lane is collapsed iff its [`LaneKey`] is in the set; the
/// resting state (nothing collapsed) is an empty set, so a brand-new session and
/// a fully-expanded one cost the same: nothing.
///
/// `generation` bumps on every mutation so the lane-panel build's staleness
/// fingerprint moves when a toggle happens (the panel must rebuild to reflect the
/// new collapse state) without having to hash the whole set each refresh.
#[derive(Debug, Clone, Default)]
pub(crate) struct LaneFoldStore {
    collapsed: HashSet<LaneKey>,
    generation: u64,
}

impl LaneFoldStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Whether the given lane is currently collapsed.
    pub(crate) fn is_collapsed(&self, key: LaneKey) -> bool {
        self.collapsed.contains(&key)
    }

    /// Toggle the given lane's collapsed state, returning the new state (`true` =
    /// now collapsed). Bumps the generation so a dependent panel build rebuilds.
    pub(crate) fn toggle(&mut self, key: LaneKey) -> bool {
        let now_collapsed = if self.collapsed.contains(&key) {
            self.collapsed.remove(&key);
            false
        } else {
            self.collapsed.insert(key);
            true
        };
        self.generation = self.generation.wrapping_add(1);
        now_collapsed
    }

    /// Collapse the given lane (idempotent). Bumps the generation only when the
    /// state actually changes, so a no-op collapse does not force a rebuild. The
    /// overlay toggles via [`Self::toggle`] and folds in bulk via
    /// [`Self::collapse_all`] / [`Self::expand_all`]; the single-lane setters are
    /// exercised directly by the unit suite (so the store's idempotence /
    /// generation contract is pinned without a terminal).
    #[cfg(test)]
    pub(crate) fn collapse(&mut self, key: LaneKey) {
        if self.collapsed.insert(key) {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// Expand the given lane (idempotent). Bumps the generation only when the
    /// state actually changes.
    #[cfg(test)]
    pub(crate) fn expand(&mut self, key: LaneKey) {
        if self.collapsed.remove(&key) {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// Collapse every lane in `keys` in one pass (the "collapse all" verb). Bumps
    /// the generation once if anything changed.
    pub(crate) fn collapse_all<I: IntoIterator<Item = LaneKey>>(&mut self, keys: I) {
        let mut changed = false;
        for key in keys {
            changed |= self.collapsed.insert(key);
        }
        if changed {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// Expand every lane in `keys` in one pass (the "expand all" verb). Bumps the
    /// generation once if anything changed.
    pub(crate) fn expand_all<I: IntoIterator<Item = LaneKey>>(&mut self, keys: I) {
        let mut changed = false;
        for key in keys {
            changed |= self.collapsed.remove(&key);
        }
        if changed {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// Number of currently-collapsed lanes in the whole store (across every
    /// entry). The overlay reports the focused entry's collapsed count via
    /// [`LanePanel::collapsed_count`]; this store-wide tally is a test/diagnostic
    /// accessor that pins the toggle/collapse-all accounting.
    #[cfg(test)]
    pub(crate) fn collapsed_count(&self) -> usize {
        self.collapsed.len()
    }

    /// The store's mutation generation, folded into the panel build fingerprint so
    /// a toggle re-projects the panel.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

/// The computed lane panel for the focused transcript entry (§12.2.2).
///
/// `entry_id` is the stable id of the entry the lanes belong to; `lanes` is the
/// ordered, foldable lane list (each already carrying its resolved collapse
/// state); `fingerprint` is the staleness tag described in the module docs;
/// `built` distinguishes "empty entry projected" from "never built" so a
/// genuinely lane-less entry is not re-walked every refresh.
#[derive(Debug, Clone, Default)]
pub(crate) struct LanePanel {
    entry_id: Option<u64>,
    lanes: Vec<Lane>,
    fingerprint: u64,
    built: bool,
}

impl LanePanel {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Fold a staleness fingerprint over the focused entry id, its lane sources,
    /// and the fold-store generation. Order- and content-sensitive: a different
    /// focused entry, a revision bump that changes a lane's line count / preview /
    /// error flag, or any collapse toggle all move the value. Pure and standalone
    /// so the caller can compute it cheaply each refresh and compare before
    /// deciding to recompute.
    pub(crate) fn fingerprint_of(
        entry_id: Option<u64>,
        lanes: &[LaneEntry],
        fold_generation: u64,
    ) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        entry_id.hash(&mut hasher);
        fold_generation.hash(&mut hasher);
        for lane in lanes {
            lane.id.hash(&mut hasher);
            lane.line_count.hash(&mut hasher);
            lane.is_error.hash(&mut hasher);
            lane.preview.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Recompute the lane panel from `entry_id` + `lanes` + the `store` collapse
    /// state **only if** `fingerprint` differs from the one captured at the last
    /// rebuild (or this is the first build). Returns `true` when a recompute
    /// actually ran, `false` when the cached panel was already current (the
    /// zero-idle-cost fast path).
    ///
    /// The caller computes `fingerprint` via [`Self::fingerprint_of`] over the
    /// same inputs. Each built [`Lane`] reads its collapsed state from `store`, so
    /// the panel reflects the persisted `(entry_id, lane_id)` fold set; a lane's
    /// `preview` is cleaned/bounded here.
    pub(crate) fn rebuild_if_stale(
        &mut self,
        fingerprint: u64,
        entry_id: Option<u64>,
        lanes: &[LaneEntry],
        store: &LaneFoldStore,
    ) -> bool {
        if self.built && fingerprint == self.fingerprint {
            return false;
        }
        self.entry_id = entry_id;
        self.lanes.clear();
        self.lanes.reserve(lanes.len());
        if let Some(id) = entry_id {
            // Emit lanes in the canonical [`LaneId::ALL`] order so they always read
            // top-to-bottom the way a turn flows (reasoning, then answer, then a
            // tool's input/output, then notice/approval/error/plan) regardless of
            // the order the caller pushed them. A lane id that appears more than
            // once in the source keeps each occurrence, in source order, within its
            // taxonomy slot.
            for canonical in LaneId::ALL.iter().copied() {
                for lane in lanes.iter().filter(|lane| lane.id == canonical) {
                    let key = LaneKey::new(id, lane.id);
                    self.lanes.push(Lane {
                        key,
                        collapsed: store.is_collapsed(key),
                        line_count: lane.line_count,
                        is_error: lane.is_error,
                        preview: clean_preview(&lane.preview),
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

    /// The id of the entry the lanes belong to, or `None` when no entry is
    /// focused / the panel is empty. Test/diagnostic accessor (the overlay drives
    /// off `lanes()` / `len()`); pins that the projection tracks the focused entry.
    #[cfg(test)]
    pub(crate) fn entry_id(&self) -> Option<u64> {
        self.entry_id
    }

    /// The built lanes in display order.
    pub(crate) fn lanes(&self) -> &[Lane] {
        &self.lanes
    }

    /// Number of lanes.
    pub(crate) fn len(&self) -> usize {
        self.lanes.len()
    }

    /// Whether the panel has any lanes.
    pub(crate) fn is_empty(&self) -> bool {
        self.lanes.is_empty()
    }

    /// The lane at list index `index`, or `None` when out of range.
    pub(crate) fn get(&self, index: usize) -> Option<&Lane> {
        self.lanes.get(index)
    }

    /// Every lane key in the panel (for the collapse-all / expand-all verbs).
    pub(crate) fn keys(&self) -> Vec<LaneKey> {
        self.lanes.iter().map(|lane| lane.key).collect()
    }

    /// Number of currently-collapsed lanes in this panel.
    pub(crate) fn collapsed_count(&self) -> usize {
        self.lanes.iter().filter(|lane| lane.collapsed).count()
    }

    /// Number of error lanes in this panel.
    pub(crate) fn error_count(&self) -> usize {
        self.lanes.iter().filter(|lane| lane.is_error).count()
    }

    /// The list index of the next lane strictly after `after` (wrapping to the
    /// first when `after` is the last or `None`). `None` only when there are no
    /// lanes. The overlay's ↑↓ move the cursor with saturating arithmetic; this
    /// wrapping verb is pinned by the unit suite so a future "next lane" idiom has
    /// the same contract as the turn outline's `next_index`.
    #[cfg(test)]
    pub(crate) fn next_index(&self, after: Option<usize>) -> Option<usize> {
        if self.lanes.is_empty() {
            return None;
        }
        match after {
            Some(i) if i + 1 < self.lanes.len() => Some(i + 1),
            Some(_) => Some(0),
            None => Some(0),
        }
    }

    /// The list index of the previous lane strictly before `before` (wrapping to
    /// the last). `None` only when there are no lanes. Drives backward lane-cursor
    /// navigation.
    #[cfg(test)]
    pub(crate) fn prev_index(&self, before: Option<usize>) -> Option<usize> {
        if self.lanes.is_empty() {
            return None;
        }
        match before {
            Some(0) | None => Some(self.lanes.len() - 1),
            Some(i) if i <= self.lanes.len() => Some(i - 1),
            Some(_) => Some(self.lanes.len() - 1),
        }
    }

    /// A compact one-line summary of the panel for the overlay header, e.g.
    /// `"4 lanes \u{00b7} 1 collapsed \u{00b7} 1 error"`. Empty string when the
    /// panel is empty.
    pub(crate) fn summary(&self) -> String {
        if self.lanes.is_empty() {
            return String::new();
        }
        let total = self.lanes.len();
        let total_word = if total == 1 { "lane" } else { "lanes" };
        let mut parts = vec![format!("{total} {total_word}")];
        let collapsed = self.collapsed_count();
        if collapsed > 0 {
            parts.push(format!("{collapsed} collapsed"));
        }
        let errors = self.error_count();
        if errors > 0 {
            let error_word = if errors == 1 { "error" } else { "errors" };
            parts.push(format!("{errors} {error_word}"));
        }
        parts.join(" \u{00b7} ")
    }
}

#[cfg(test)]
#[path = "lane_fold_tests.rs"]
mod tests;
