use super::*;

#[test]
fn new_groups_is_empty() {
    let groups = QueueGroups::new();
    assert!(groups.is_empty());
    assert_eq!(groups.len(), 0);
    assert!(groups.groups().is_empty());
}

#[test]
fn create_group_assigns_default_name_and_members() {
    let mut groups = QueueGroups::new();
    let gid = groups.create_group(&[1, 2, 3]).expect("created");
    assert_eq!(groups.len(), 1);
    let g = groups.groups().first().expect("group");
    assert_eq!(g.id, gid);
    assert_eq!(g.name, "Group 1");
    assert_eq!(g.members.len(), 3);
    assert!(!g.paused);
    assert!(!g.collapsed);
    // Each member resolves back to this group.
    for id in [1, 2, 3] {
        assert_eq!(groups.group_id_of_item(id), Some(gid));
    }
    // A loose id belongs to no group.
    assert_eq!(groups.group_id_of_item(9), None);
}

#[test]
fn create_group_with_no_members_is_noop() {
    let mut groups = QueueGroups::new();
    assert_eq!(groups.create_group(&[]), None);
    assert!(groups.is_empty());
}

#[test]
fn group_numbers_increment_across_creations() {
    let mut groups = QueueGroups::new();
    groups.create_group(&[1]).expect("g1");
    groups.create_group(&[2]).expect("g2");
    let names: Vec<&str> = groups.groups().iter().map(|g| g.name.as_str()).collect();
    assert_eq!(names, vec!["Group 1", "Group 2"]);
}

#[test]
fn item_joins_only_one_group_at_a_time() {
    let mut groups = QueueGroups::new();
    let g1 = groups.create_group(&[1, 2]).expect("g1");
    // Re-grouping item 2 into a new group pulls it out of g1.
    let g2 = groups.create_group(&[2, 3]).expect("g2");
    assert_eq!(groups.group_id_of_item(2), Some(g2));
    assert_eq!(groups.group_id_of_item(1), Some(g1));
    assert_eq!(groups.group_id_of_item(3), Some(g2));
    // g1 still exists (still owns item 1).
    assert_eq!(groups.len(), 2);
}

#[test]
fn regrouping_the_last_member_drops_the_emptied_group() {
    let mut groups = QueueGroups::new();
    groups.create_group(&[1]).expect("g1");
    // Move item 1 into a fresh group; g1 is now empty and is dropped.
    let g2 = groups.create_group(&[1, 2]).expect("g2");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups.group_id_of_item(1), Some(g2));
}

#[test]
fn dissolve_returns_members_loose() {
    let mut groups = QueueGroups::new();
    let gid = groups.create_group(&[1, 2]).expect("g");
    assert!(groups.dissolve(gid));
    assert!(groups.is_empty());
    assert_eq!(groups.group_id_of_item(1), None);
    assert_eq!(groups.group_id_of_item(2), None);
    // Dissolving a gone group is a no-op.
    assert!(!groups.dissolve(gid));
}

#[test]
fn toggle_paused_flips_and_drain_gate_follows() {
    let mut groups = QueueGroups::new();
    let gid = groups.create_group(&[1, 2]).expect("g");
    assert!(!groups.is_item_paused(1));
    assert_eq!(groups.toggle_paused(gid), Some(true));
    assert!(groups.is_item_paused(1));
    assert!(groups.is_item_paused(2));
    // A loose item is never paused.
    assert!(!groups.is_item_paused(9));
    assert_eq!(groups.toggle_paused(gid), Some(false));
    assert!(!groups.is_item_paused(1));
    // Toggling a gone group reports None.
    assert_eq!(groups.toggle_paused(999), None);
}

#[test]
fn toggle_collapsed_flips() {
    let mut groups = QueueGroups::new();
    let gid = groups.create_group(&[1]).expect("g");
    assert!(!groups.group_of_item(1).expect("g").collapsed);
    assert_eq!(groups.toggle_collapsed(gid), Some(true));
    assert!(groups.group_of_item(1).expect("g").collapsed);
    assert_eq!(groups.toggle_collapsed(gid), Some(false));
    assert!(!groups.group_of_item(1).expect("g").collapsed);
    assert_eq!(groups.toggle_collapsed(999), None);
}

#[test]
fn retain_live_prunes_drained_members_and_empty_groups() {
    let mut groups = QueueGroups::new();
    groups.create_group(&[1, 2]).expect("g1");
    groups.create_group(&[3]).expect("g2");
    // Items 2 and 3 drained out; only 1 and 4 are live.
    groups.retain_live(&[1, 4]);
    // g1 keeps item 1; g2 lost its only member and is dropped.
    assert_eq!(groups.len(), 1);
    assert_eq!(groups.group_id_of_item(1), Some(groups.groups()[0].id));
    assert_eq!(groups.group_id_of_item(3), None);
    assert!(!groups.groups()[0].members.contains(&2));
}

#[test]
fn groups_summary_reports_counts_and_flags() {
    let mut groups = QueueGroups::new();
    let g1 = groups.create_group(&[1, 2]).expect("g1");
    groups.create_group(&[3, 4, 5]).expect("g2");
    groups.toggle_paused(g1);
    let summary = groups_summary(&groups);
    assert_eq!(summary, "Group 1 (2, paused) · Group 2 (3)");
}

#[test]
fn groups_summary_is_empty_with_no_groups() {
    let groups = QueueGroups::new();
    assert_eq!(groups_summary(&groups), "");
}

#[test]
fn group_marker_glyph_reflects_state() {
    let mut groups = QueueGroups::new();
    let gid = groups.create_group(&[1]).expect("g");
    assert_eq!(group_marker_glyph(None), "   ");
    assert_eq!(group_marker_glyph(groups.group_of_item(1)), "[G]");
    groups.toggle_paused(gid);
    assert_eq!(group_marker_glyph(groups.group_of_item(1)), "[P]");
    // The styled span content mirrors the glyph (the colour is theme-driven).
    assert_eq!(
        group_marker_span(groups.group_of_item(1)).content.as_ref(),
        "[P]"
    );
    assert_eq!(group_marker_span(None).content.as_ref(), "   ");
}
