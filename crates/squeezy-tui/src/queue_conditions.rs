//! Conditional Queue Items (§12.3.5).
//!
//! By default every queued prompt ([`crate::prompt_queue`]) is unconditional:
//! the drain pump pops the front runnable item and runs it the moment the
//! previous turn finishes. Conditional Queue Items lets the user attach a simple
//! *run-condition* to an individual queued prompt so it only dispatches when the
//! condition is satisfied by the outcome of the turn that just ran — e.g. "only
//! if the previous turn succeeded", "only if it failed", "only if it edited
//! files", or "manual only" (never auto-drained). A prompt whose condition is not
//! satisfied is *skipped* (dropped from the queue) rather than parked, and a
//! manual-only prompt is *blocked* (held in place until the user runs it by hand
//! via run-next), so hidden automation never silently runs the wrong prompt.
//!
//! Like [`crate::queue_groups`] and [`crate::prompt_queue_multiselect`], this
//! module is the *pure-state* surface: it owns the per-item condition records and
//! the evaluation math, addressed by the stable per-item id from
//! `TuiApp::prompt_queue_ids` (the same id the hit-test registry, the drag/delete
//! paths, the multi-select set, and the group model key off). Identity is the id,
//! never a Vec position, so a reorder, a front-drain, or a delete between setting
//! a condition and evaluating it can never make the gate consult the wrong row;
//! ids that have since drained out simply drop from the map.
//!
//! The live queue, the drain pump, and the turn lifecycle all live on `TuiApp`,
//! so the lib.rs handlers do the actual mutation and capture the turn outcome.
//! Keeping the logic here (and pure) means the keyboard and mouse paths share one
//! source of truth, the gate is consulted only during the drain (never the render
//! path, so idle cost stays zero), and the tests pin every condition against a
//! synthetic outcome without a running terminal.

use std::collections::HashMap;

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

/// The run-condition attached to a queued prompt. `Always` is the implicit
/// default for any item with no entry in [`QueueConditions`], so the common case
/// (an unconditional queue) carries no state and behaves exactly as before.
///
/// Each variant maps to reliably-available turn-outcome metadata (success /
/// failure / file edits), plus a `Manual` escape hatch that never auto-drains.
/// The set is deliberately small and deterministic — no regex engine, no output
/// scraping — so the gate is cheap and its behaviour is obvious to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum QueueCondition {
    /// Run unconditionally as soon as the prompt reaches the front (the default).
    #[default]
    Always,
    /// Run only if the previous turn succeeded; skip it otherwise.
    IfPrevSucceeded,
    /// Run only if the previous turn failed (errored or was cancelled); skip it
    /// otherwise. Lets a recovery / retry prompt fire exactly when something
    /// went wrong.
    IfPrevFailed,
    /// Run only if the previous turn edited at least one file; skip it otherwise.
    IfPrevEdited,
    /// Run only if the previous turn edited no files; skip it otherwise.
    IfPrevNoEdits,
    /// Never auto-drain: the prompt is held in place (blocked) until the user
    /// runs it by hand (run-next). A safe "staging" slot for a prompt the user
    /// wants queued but not fired automatically.
    Manual,
}

impl QueueCondition {
    /// The ordered cycle the editor steps through (Always → succeeded → failed →
    /// edited → no-edits → manual → Always). Shared by the keyboard `v` verb and
    /// the mouse twin so both advance through the same sequence.
    const CYCLE: [QueueCondition; 6] = [
        QueueCondition::Always,
        QueueCondition::IfPrevSucceeded,
        QueueCondition::IfPrevFailed,
        QueueCondition::IfPrevEdited,
        QueueCondition::IfPrevNoEdits,
        QueueCondition::Manual,
    ];

    /// The next condition in the editor cycle. Wraps from `Manual` back to
    /// `Always`. Pure so the keyboard and mouse paths step identically.
    pub(crate) fn next(self) -> QueueCondition {
        let pos = Self::CYCLE.iter().position(|c| *c == self).unwrap_or(0);
        Self::CYCLE[(pos + 1) % Self::CYCLE.len()]
    }

    /// Whether this is the default unconditional run. The map never stores an
    /// `Always` entry (setting a row back to `Always` clears it), so this is only
    /// used to recognise the no-op end of the cycle.
    pub(crate) fn is_always(self) -> bool {
        matches!(self, QueueCondition::Always)
    }

    /// A compact glyph painted at the head of a conditional overlay row. An
    /// unconditional row gets blanks so columns stay aligned with the group /
    /// multi-select markers; a conditional row gets a tag reflecting the kind.
    /// Kept next to the state so the render and the tests agree on the glyph.
    pub(crate) fn marker_glyph(self) -> &'static str {
        match self {
            QueueCondition::Always => "   ",
            QueueCondition::IfPrevSucceeded => "[✓]",
            QueueCondition::IfPrevFailed => "[✗]",
            QueueCondition::IfPrevEdited => "[±]",
            QueueCondition::IfPrevNoEdits => "[=]",
            QueueCondition::Manual => "[⏷]",
        }
    }

    /// A self-describing ASCII glyph for a no-color terminal, where every theme
    /// colour collapses to one foreground and the tinted Unicode tags
    /// ([`Self::marker_glyph`]) lose the runnable/skip/blocked distinction they
    /// carried. Each kind reads on its letter alone — `[y]`/`[n]` for the
    /// succeeded/failed gates, `[+]`/`[0]` for edited/no-edits, `[m]` for manual,
    /// and a quiet `[ ]` for an always-run row — so a monochrome scan still tells
    /// the gates apart. Exactly three cells wide so the column never shifts.
    pub(crate) fn marker_glyph_ascii(self) -> &'static str {
        match self {
            QueueCondition::Always => "[ ]",
            QueueCondition::IfPrevSucceeded => "[y]",
            QueueCondition::IfPrevFailed => "[n]",
            QueueCondition::IfPrevEdited => "[+]",
            QueueCondition::IfPrevNoEdits => "[0]",
            QueueCondition::Manual => "[m]",
        }
    }

    /// A short human label for the status line when the condition is set / cycled.
    pub(crate) fn label(self) -> &'static str {
        match self {
            QueueCondition::Always => "always",
            QueueCondition::IfPrevSucceeded => "if previous succeeded",
            QueueCondition::IfPrevFailed => "if previous failed",
            QueueCondition::IfPrevEdited => "if previous edited files",
            QueueCondition::IfPrevNoEdits => "if previous made no edits",
            QueueCondition::Manual => "manual only",
        }
    }
}

/// The outcome of the turn that just finished, captured at turn-finish so the
/// drain gate can evaluate the next item's condition against it. `None` on
/// `TuiApp` means no turn has finished yet this session — the gate then treats a
/// previous-turn condition as not-yet-satisfiable (blocked), never as a false
/// match, so a "run only if previous succeeded" prompt at the very front of a
/// fresh session waits rather than skipping or running blind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TurnOutcome {
    /// Whether the turn ended successfully (vs. errored or cancelled).
    pub(crate) succeeded: bool,
    /// Whether the turn edited at least one file.
    pub(crate) had_edits: bool,
}

/// The evaluation result for one queued prompt's condition against the latest
/// [`TurnOutcome`]. The drain gate acts on this; the render path uses the same
/// math (over the cached last outcome) only to *label* a row, so paint and
/// policy never drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConditionEval {
    /// The condition is satisfied (or there is none): dispatch the prompt now.
    Runnable,
    /// The condition is not satisfied and never will be for this prompt at the
    /// front (e.g. "if succeeded" after a failed turn): the gate *skips* the
    /// prompt, dropping it from the queue.
    Skipped,
    /// The condition cannot be evaluated yet (a previous-turn condition with no
    /// finished turn to read), or the prompt is `Manual`: the gate holds it in
    /// place (does not drain, does not drop) until the situation changes or the
    /// user runs it by hand.
    Blocked,
}

/// The per-item run-conditions layered over the flat prompt queue.
///
/// Empty means "every queued prompt is unconditional" — the queue behaves
/// exactly as it did before this feature and carries zero extra state. A prompt
/// has at most one condition; setting it back to `Always` removes the entry so
/// the map only ever holds genuinely-conditional rows.
#[derive(Debug, Clone, Default)]
pub(crate) struct QueueConditions {
    /// Stable item id → its non-`Always` condition. An id absent from the map is
    /// `Always` (unconditional). Never holds an `Always` value.
    by_id: HashMap<u64, QueueCondition>,
}

impl QueueConditions {
    pub(crate) fn new() -> Self {
        Self {
            by_id: HashMap::new(),
        }
    }

    /// Whether no item carries a condition (the queue is fully unconditional).
    /// Used by the render path to skip the condition summary entirely when the
    /// queue is plain, and by tests to assert the map clears.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// The condition attached to queue item `id` (`Always` when unset).
    pub(crate) fn get(&self, id: u64) -> QueueCondition {
        self.by_id.get(&id).copied().unwrap_or_default()
    }

    /// Set queue item `id`'s condition. Storing `Always` clears the entry so the
    /// map only ever holds genuinely-conditional rows.
    pub(crate) fn set(&mut self, id: u64, condition: QueueCondition) {
        if condition.is_always() {
            self.by_id.remove(&id);
        } else {
            self.by_id.insert(id, condition);
        }
    }

    /// Advance queue item `id` to the next condition in the editor cycle and
    /// return the new value. The keyboard `v` verb and the mouse twin both call
    /// this, so they step through the identical sequence.
    pub(crate) fn cycle(&mut self, id: u64) -> QueueCondition {
        let next = self.get(id).next();
        self.set(id, next);
        next
    }

    /// Drop conditions for ids no longer present in the live queue (drained or
    /// deleted). Called whenever the queue may have shifted under the overlay so a
    /// stale id can never leave a phantom condition behind and so the drain gate
    /// never consults a condition for a prompt that no longer exists. Pure over
    /// the live id set; cheap (a single retain) and a no-op when nothing is
    /// conditional.
    pub(crate) fn retain_live(&mut self, live_ids: &[u64]) {
        if self.by_id.is_empty() {
            return;
        }
        self.by_id.retain(|id, _| live_ids.contains(id));
    }
}

/// Evaluate `condition` against the latest turn `outcome` (or `None` when no turn
/// has finished this session). The single decision the drain gate consults and
/// the render path mirrors for labels, so policy and paint can never diverge.
///
/// * `Always` is always [`ConditionEval::Runnable`].
/// * `Manual` is always [`ConditionEval::Blocked`] (never auto-drains).
/// * A previous-turn condition with no `outcome` yet is [`ConditionEval::Blocked`]
///   — a fresh session waits rather than skipping or running blind.
/// * Otherwise the predicate either matches ([`ConditionEval::Runnable`]) or does
///   not ([`ConditionEval::Skipped`]).
pub(crate) fn evaluate(condition: QueueCondition, outcome: Option<TurnOutcome>) -> ConditionEval {
    match condition {
        QueueCondition::Always => ConditionEval::Runnable,
        QueueCondition::Manual => ConditionEval::Blocked,
        cond => {
            let Some(outcome) = outcome else {
                // A previous-turn condition with nothing to read yet: hold the
                // prompt rather than skip it or run it blind.
                return ConditionEval::Blocked;
            };
            let matched = match cond {
                QueueCondition::IfPrevSucceeded => outcome.succeeded,
                QueueCondition::IfPrevFailed => !outcome.succeeded,
                QueueCondition::IfPrevEdited => outcome.had_edits,
                QueueCondition::IfPrevNoEdits => !outcome.had_edits,
                // Always / Manual handled above; unreachable here.
                QueueCondition::Always | QueueCondition::Manual => true,
            };
            if matched {
                ConditionEval::Runnable
            } else {
                ConditionEval::Skipped
            }
        }
    }
}

/// What the drain pump should do with the queue *right now*, computed by
/// [`plan_drain`] from the live front-to-back item list and the latest outcome.
///
/// The pump applies this one step at a time: it [`Run`](DrainAction::Run)s or
/// [`Drop`](DrainAction::Drop)s the named index and re-plans, or [`Stop`]s when
/// nothing at the front is dispatchable. Pure so the policy — including the order
/// in which paused / blocked / skip-bound items are stepped over — is pinned by
/// tests without a live queue or a running terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrainAction {
    /// Dispatch the prompt at this index now (a turn is free to start).
    Run(usize),
    /// Drop the prompt at this index (its "if previous X" condition will never be
    /// satisfied for this outcome) and re-plan. The pump removes it and loops.
    Drop(usize),
    /// Nothing is dispatchable: every remaining front-to-back item is parked by a
    /// paused group or a blocked (manual / not-yet-evaluable) condition. The pump
    /// stops until the situation changes.
    Stop,
}

/// One queued item's drain-relevant facts, in live queue order. `paused` is the
/// Queue-Groups (§12.3.4) pause flag; `condition` is this item's run-condition
/// (§12.3.5). Kept as a tiny POD so [`plan_drain`] is pure over a slice the caller
/// builds from the id sidecar + the two state maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DrainItem {
    /// Whether the item is held back by a paused group.
    pub(crate) paused: bool,
    /// The item's run-condition.
    pub(crate) condition: QueueCondition,
}

/// Decide the single next drain step over `items` (live queue order) given the
/// latest turn `outcome`.
///
/// Walks front-to-back and classifies each item:
/// * a paused-group item is *parked* — stepped over, the search continues behind
///   it (matching the pre-existing paused-group drain behaviour);
/// * a [`ConditionEval::Blocked`] item (manual, or a previous-turn condition with
///   no finished turn yet) is *parked* the same way;
/// * a [`ConditionEval::Skipped`] item is the answer: [`DrainAction::Drop`] it
///   (the caller removes it and re-plans, so a run of skip-bound prompts clears in
///   order);
/// * a [`ConditionEval::Runnable`] item is the answer: [`DrainAction::Run`] it.
///
/// Returns [`DrainAction::Stop`] when every item is parked. The first *actionable*
/// item (run or drop) wins, so a skip-bound prompt ahead of a runnable one is
/// cleared first (one [`Drop`] per call), exactly as if the user had deleted it.
pub(crate) fn plan_drain(items: &[DrainItem], outcome: Option<TurnOutcome>) -> DrainAction {
    for (index, item) in items.iter().enumerate() {
        if item.paused {
            // Held back by a paused group: park it and look behind it.
            continue;
        }
        match evaluate(item.condition, outcome) {
            ConditionEval::Runnable => return DrainAction::Run(index),
            ConditionEval::Skipped => return DrainAction::Drop(index),
            ConditionEval::Blocked => continue,
        }
    }
    DrainAction::Stop
}

/// The marker glyph + colour painted at the head of a conditional overlay row.
/// An unconditional row gets a quiet `[·]` placeholder (so every row shows a
/// complete, consistent condition column that reads as "always-run", not a blank
/// hole); a conditional row gets a tag tinted by its evaluation against the
/// latest outcome so a blocked / skip-bound row reads at a glance: a row that
/// *would* skip on the next drain is drawn in the warn colour, a blocked (manual
/// / not-yet-evaluable) row in quiet, a runnable one in accent.
///
/// On a no-color terminal (`NO_COLOR` / a monochrome term) every theme colour
/// collapses to one foreground, so the tint that distinguished runnable from
/// skip-bound from blocked is gone. There we fall back to the self-describing
/// ASCII glyphs ([`QueueCondition::marker_glyph_ascii`]) so the gate kinds stay
/// legible on the letter alone. Kept here next to the state so the render and the
/// tests agree on the glyph.
pub(crate) fn condition_marker_span(
    condition: QueueCondition,
    outcome: Option<TurnOutcome>,
) -> Span<'static> {
    let no_color = matches!(
        crate::render::palette::color_level(),
        crate::render::palette::ColorLevel::NoColor
    );
    let glyph = if no_color {
        condition.marker_glyph_ascii()
    } else if condition.is_always() {
        // A quiet placeholder so an always-run row reads as "set to run", not a
        // missing-data hole, while staying the same 3-cell width as the tags.
        "[·]"
    } else {
        condition.marker_glyph()
    };
    let color = if condition.is_always() {
        crate::render::theme::quiet()
    } else {
        match evaluate(condition, outcome) {
            ConditionEval::Runnable => crate::render::theme::accent(),
            ConditionEval::Skipped => crate::render::theme::warn(),
            ConditionEval::Blocked => crate::render::theme::quiet(),
        }
    };
    Span::styled(
        glyph,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

#[cfg(test)]
#[path = "queue_conditions_tests.rs"]
mod tests;
