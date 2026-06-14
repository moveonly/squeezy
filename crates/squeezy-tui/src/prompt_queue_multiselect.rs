//! Multi-select for the prompt-queue reorder overlay (§11G.7).
//!
//! The base overlay ([`crate::prompt_queue::PromptQueueState`]) carries a single
//! focus cursor. This module adds a *group* selection layered on top: the user
//! tags several queued prompts, then deletes them as one undoable batch, moves
//! them as a contiguous block, or merges them into the composer.
//!
//! Identity is the stable per-item id from `TuiApp::prompt_queue_ids` (the same
//! id the hit-test registry and the drag/delete paths key off), NOT a Vec
//! position. A reorder, a front-drain, or a delete between tagging and acting
//! therefore can never make a group op touch the wrong row: every selected id
//! is re-resolved to a live index at action time, and ids that have since
//! drained out simply drop from the set.
//!
//! This module is the *pure-state* surface: it owns nothing but a set of ids
//! and the math that turns that set + the live id order into ordered indices.
//! The live queue, the composer, and the undo stack all live on `TuiApp`, so
//! the lib.rs handlers do the actual mutation. Keeping the logic here (and
//! pure) means the keyboard and mouse paths share one source of truth and the
//! tests pin it without a running terminal.

use std::collections::BTreeSet;

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

/// The set of queued prompts tagged for a group operation, addressed by their
/// stable ids. Empty means "no multi-selection active" — group verbs then fall
/// back to the single focused row, exactly as before this feature.
#[derive(Debug, Clone, Default)]
pub(crate) struct MultiSelect {
    selected: BTreeSet<u64>,
}

impl MultiSelect {
    pub(crate) fn new() -> Self {
        Self {
            selected: BTreeSet::new(),
        }
    }

    /// Whether `id` is currently tagged.
    pub(crate) fn contains(&self, id: u64) -> bool {
        self.selected.contains(&id)
    }

    /// Whether nothing is tagged (the group verbs fall back to the focus row).
    pub(crate) fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    /// How many ids are tagged.
    pub(crate) fn len(&self) -> usize {
        self.selected.len()
    }

    /// Toggle `id` in/out of the selection. Returns the new membership so the
    /// caller can phrase a status line ("added"/"removed").
    pub(crate) fn toggle(&mut self, id: u64) -> bool {
        if self.selected.remove(&id) {
            false
        } else {
            self.selected.insert(id);
            true
        }
    }

    /// Tag every id in `ids` (the full live queue order). Idempotent.
    pub(crate) fn select_all(&mut self, ids: &[u64]) {
        self.selected.extend(ids.iter().copied());
    }

    /// Drop everything from the selection.
    pub(crate) fn clear(&mut self) {
        self.selected.clear();
    }

    /// Drop ids that are no longer present in the live queue (drained or
    /// deleted outside the overlay). Called whenever the queue may have shifted
    /// under the overlay so a stale id can never make a group op a partial
    /// no-op that silently skips a row.
    pub(crate) fn retain_live(&mut self, live_ids: &[u64]) {
        self.selected.retain(|id| live_ids.contains(id));
    }

    /// The tagged ids in *live queue order* (front-to-back), filtered to those
    /// still present. This is the canonical order a group delete removes in and
    /// a merge concatenates in, so the composer text reads top-to-bottom like
    /// the overlay shows. `live_ids` is the queue's id sidecar in order.
    pub(crate) fn ids_in_queue_order(&self, live_ids: &[u64]) -> Vec<u64> {
        live_ids
            .iter()
            .copied()
            .filter(|id| self.selected.contains(id))
            .collect()
    }

    /// Borrow the raw id set. Used by [`move_group`] (and the tests) to do the
    /// block-move math without copying the set each frame.
    pub(crate) fn set(&self) -> &BTreeSet<u64> {
        &self.selected
    }
}

/// Direction of a group move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MoveDir {
    Up,
    Down,
}

/// Compute the queue order after moving the tagged group one step in `dir`.
///
/// `ids` is the live queue id order (front-to-back); `selected` is the set of
/// tagged ids. The group keeps its internal order and moves as a block by one
/// slot, sliding past the single unselected neighbour on the leading edge. The
/// block is blocked (no-op) once its leading edge reaches the queue boundary,
/// so it never wraps or escapes. Returns the new id order, or `None` when the
/// move is a no-op (nothing tagged, or the group is already flush against the
/// boundary in `dir`) so the caller can skip recording an undo.
///
/// Pure index math over `ids`, so it is identical for the keyboard verb and a
/// future mouse twin and is pinned directly by tests without a live queue.
pub(crate) fn move_group(ids: &[u64], selected: &BTreeSet<u64>, dir: MoveDir) -> Option<Vec<u64>> {
    if selected.is_empty() {
        return None;
    }
    let n = ids.len();
    let is_sel = |id: u64| selected.contains(&id);
    // Indices of the tagged rows, ascending. Filtered to live ids only.
    let sel_idx: Vec<usize> = (0..n).filter(|&i| is_sel(ids[i])).collect();
    if sel_idx.is_empty() {
        return None;
    }
    match dir {
        MoveDir::Up => {
            // Blocked if the topmost tagged row is already at the front.
            if sel_idx[0] == 0 {
                return None;
            }
            let mut out = ids.to_vec();
            // Top-to-bottom: each tagged row swaps with the row above it. The
            // row above the topmost is unselected (else it'd be in sel_idx and
            // already shifted), so the block slides up one as a unit.
            for &i in &sel_idx {
                out.swap(i, i - 1);
            }
            Some(out)
        }
        MoveDir::Down => {
            // Blocked if the bottommost tagged row is already at the back.
            if *sel_idx.last().expect("non-empty") == n - 1 {
                return None;
            }
            let mut out = ids.to_vec();
            // Bottom-to-top so each swap-down lands on a still-unselected slot.
            for &i in sel_idx.iter().rev() {
                out.swap(i, i + 1);
            }
            Some(out)
        }
    }
}

/// The marker glyph painted at the head of an overlay row to show its
/// multi-select state. A tagged row gets a filled box `[x]`, an untagged row an
/// empty one `[ ]` (so columns stay aligned). Kept here next to the state so the
/// render and the tests agree on the exact glyph.
pub(crate) fn marker_glyph(is_tagged: bool) -> &'static str {
    if is_tagged { "[x]" } else { "[ ]" }
}

/// Style the multi-select marker: a tagged row is drawn in the accent colour
/// and bold so the group reads at a glance; an untagged row is quiet.
pub(crate) fn marker_span(is_tagged: bool) -> Span<'static> {
    let style = if is_tagged {
        Style::default()
            .fg(crate::render::theme::accent())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(crate::render::theme::quiet())
    };
    Span::styled(marker_glyph(is_tagged), style)
}

#[cfg(test)]
#[path = "prompt_queue_multiselect_tests.rs"]
mod tests;
