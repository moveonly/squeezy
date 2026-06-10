//! Queue Groups (§12.3.4).
//!
//! The prompt queue ([`crate::prompt_queue`]) is a flat list of prompts the
//! user typed while a turn was running, draining one-at-a-time as each turn
//! finishes. Queue Groups turns that flat list into deliberate *batches*: the
//! user tags several queued prompts (the §11G.7 multi-select machinery), then
//! folds them into one named group that can be collapsed, paused/resumed, or
//! dissolved as a unit. A paused group's items are skipped by the drain pump,
//! so a whole batch can be held back without deleting it.
//!
//! Like [`crate::prompt_queue_multiselect`], this module is the *pure-state*
//! surface: it owns nothing but the group records and the math that maps the
//! live queue's stable id order onto them. Identity is the stable per-item id
//! from `TuiApp::prompt_queue_ids` (the same id the hit-test registry, the
//! drag/delete paths, and the multi-select set key off), NOT a Vec position. A
//! reorder, a front-drain, or a delete between grouping and acting therefore can
//! never make a group op touch the wrong row: a group remembers its members by
//! id, and ids that have since drained out simply drop from the group.
//!
//! The live queue, the composer, and the drain pump all live on `TuiApp`, so the
//! lib.rs handlers do the actual mutation. Keeping the logic here (and pure)
//! means the keyboard and mouse paths share one source of truth and the tests
//! pin it without a running terminal.

use std::collections::BTreeSet;

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

/// A named batch of queued prompts, addressed by the stable ids of its members.
///
/// `collapsed` hides the group's members behind its header row in the overlay;
/// `paused` holds the whole batch back from the drain pump. Both are pure UI /
/// policy flags — the prompts themselves stay in `TuiApp::prompt_queue` in queue
/// order regardless of which group (if any) owns them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueueGroup {
    /// Stable, never-reused id naming this group for its whole lifetime. Lets
    /// the overlay address a group without a Vec position the way item ids do.
    pub(crate) id: u64,
    /// User-facing label. Defaults to `Group N`; renamed via the overlay.
    pub(crate) name: String,
    /// The member prompts, by their stable queue-item ids. A member that drains
    /// or is deleted is pruned by [`QueueGroups::retain_live`], so the set never
    /// names a row that no longer exists.
    pub(crate) members: BTreeSet<u64>,
    /// When true the members are hidden behind the header row in the overlay.
    pub(crate) collapsed: bool,
    /// When true the drain pump skips every member, holding the batch back until
    /// the user resumes it. The prompts are not deleted — just parked.
    pub(crate) paused: bool,
}

/// The collection of queue groups layered over the flat prompt queue.
///
/// Empty means "no grouping active" — every prompt is loose and the queue
/// behaves exactly as it did before this feature. A queued prompt belongs to at
/// most one group (forming a new group from an item first pulls it out of any
/// group it was already in), so the membership math is unambiguous.
#[derive(Debug, Clone, Default)]
pub(crate) struct QueueGroups {
    groups: Vec<QueueGroup>,
    /// Monotonic source for `QueueGroup::id`. Never reused.
    next_group_id: u64,
    /// Monotonic counter feeding the default `Group N` label, so a dissolved
    /// group's number is never silently reused while the session lives.
    next_group_number: u64,
}

impl QueueGroups {
    pub(crate) fn new() -> Self {
        Self {
            groups: Vec::new(),
            next_group_id: 0,
            next_group_number: 1,
        }
    }

    /// Whether no groups exist (the queue behaves as a flat list).
    pub(crate) fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// How many groups exist.
    pub(crate) fn len(&self) -> usize {
        self.groups.len()
    }

    /// Borrow the groups in creation order.
    pub(crate) fn groups(&self) -> &[QueueGroup] {
        &self.groups
    }

    /// The group owning queue item `id`, if any.
    pub(crate) fn group_of_item(&self, id: u64) -> Option<&QueueGroup> {
        self.groups.iter().find(|g| g.members.contains(&id))
    }

    /// The id of the group owning queue item `id`, if any.
    pub(crate) fn group_id_of_item(&self, id: u64) -> Option<u64> {
        self.group_of_item(id).map(|g| g.id)
    }

    /// Whether queue item `id` is in a group that is currently paused. The drain
    /// pump asks this per item, so a paused group's prompts are skipped while
    /// loose / running-group prompts keep draining. An item in no group is never
    /// paused.
    pub(crate) fn is_item_paused(&self, id: u64) -> bool {
        self.group_of_item(id).is_some_and(|g| g.paused)
    }

    /// Form a new group from `member_ids` (already filtered to live, in queue
    /// order). Any member that was in another group is first removed from it, so
    /// the new group is the item's sole owner; a group emptied that way is
    /// dropped. Returns the new group's id, or `None` when `member_ids` is empty
    /// (nothing to group).
    pub(crate) fn create_group(&mut self, member_ids: &[u64]) -> Option<u64> {
        if member_ids.is_empty() {
            return None;
        }
        // Pull every new member out of whatever group it was in, so a prompt is
        // never double-owned.
        for &id in member_ids {
            self.remove_member_everywhere(id);
        }
        let id = self.next_group_id;
        self.next_group_id += 1;
        let number = self.next_group_number;
        self.next_group_number += 1;
        let members: BTreeSet<u64> = member_ids.iter().copied().collect();
        self.groups.push(QueueGroup {
            id,
            name: format!("Group {number}"),
            members,
            collapsed: false,
            paused: false,
        });
        Some(id)
    }

    /// Dissolve the group with `group_id`, returning its members loose into the
    /// queue (the prompts are untouched — only the grouping is removed). Returns
    /// `true` if a group was dissolved.
    pub(crate) fn dissolve(&mut self, group_id: u64) -> bool {
        let before = self.groups.len();
        self.groups.retain(|g| g.id != group_id);
        self.groups.len() != before
    }

    /// Toggle the collapsed state of the group with `group_id`. Returns the new
    /// state, or `None` if the group is gone.
    pub(crate) fn toggle_collapsed(&mut self, group_id: u64) -> Option<bool> {
        let group = self.groups.iter_mut().find(|g| g.id == group_id)?;
        group.collapsed = !group.collapsed;
        Some(group.collapsed)
    }

    /// Toggle the paused state of the group with `group_id`. Returns the new
    /// state, or `None` if the group is gone.
    pub(crate) fn toggle_paused(&mut self, group_id: u64) -> Option<bool> {
        let group = self.groups.iter_mut().find(|g| g.id == group_id)?;
        group.paused = !group.paused;
        Some(group.paused)
    }

    /// Drop members that are no longer present in the live queue (drained or
    /// deleted), and drop any group left empty as a result. Called whenever the
    /// queue may have shifted under the overlay so a stale id can never make a
    /// group op a partial no-op that silently skips a row, and so the drain pump
    /// never consults a group that only names vanished prompts.
    pub(crate) fn retain_live(&mut self, live_ids: &[u64]) {
        let live: BTreeSet<u64> = live_ids.iter().copied().collect();
        for group in &mut self.groups {
            group.members.retain(|id| live.contains(id));
        }
        self.groups.retain(|g| !g.members.is_empty());
    }

    /// Remove queue item `id` from whatever group owns it (no-op if loose),
    /// dropping a group emptied that way. Used by [`create_group`] to keep the
    /// one-group-per-item invariant.
    fn remove_member_everywhere(&mut self, id: u64) {
        for group in &mut self.groups {
            group.members.remove(&id);
        }
        self.groups.retain(|g| !g.members.is_empty());
    }
}

/// A one-line summary of every group's state for the queue strip / overlay
/// header (§12.3.4: "Queue strip shows next/paused/blocked groups and item
/// counts"). Pure over the group list, so the render and the tests agree.
///
/// Reads e.g. `Group 1 (2, paused) · Group 2 (3)`. Empty when there are no
/// groups, so the caller can omit the line entirely.
pub(crate) fn groups_summary(groups: &QueueGroups) -> String {
    groups
        .groups()
        .iter()
        .map(|g| {
            let mut flags: Vec<&str> = Vec::new();
            if g.paused {
                flags.push("paused");
            }
            if g.collapsed {
                flags.push("collapsed");
            }
            let count = g.members.len();
            if flags.is_empty() {
                format!("{} ({count})", g.name)
            } else {
                format!("{} ({count}, {})", g.name, flags.join(", "))
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The marker glyph painted at the head of a grouped overlay row. A loose row
/// (no group) gets blanks so columns stay aligned with the multi-select marker;
/// a grouped row gets a compact tag reflecting paused (`⏸`) / running (`▸`)
/// state. Kept here next to the state so the render and the tests agree on the
/// exact glyph.
pub(crate) fn group_marker_glyph(group: Option<&QueueGroup>) -> &'static str {
    match group {
        None => "   ",
        Some(g) if g.paused => "[P]",
        Some(_) => "[G]",
    }
}

/// Style the group marker: a paused group is drawn in the warn colour so a
/// held-back batch reads at a glance; a running group in the accent colour; a
/// loose row is invisible (blanks).
pub(crate) fn group_marker_span(group: Option<&QueueGroup>) -> Span<'static> {
    let glyph = group_marker_glyph(group);
    let color = match group {
        None => crate::render::theme::quiet(),
        Some(g) if g.paused => crate::render::theme::warn(),
        Some(_) => crate::render::theme::accent(),
    };
    Span::styled(
        glyph,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

#[cfg(test)]
#[path = "queue_groups_tests.rs"]
mod tests;
