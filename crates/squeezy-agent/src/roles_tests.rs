use super::*;

const CONTROL_TOOL_NAMES: &[&str] = &["delegate", "explore", "delegate_plan", "delegate_review"];

#[test]
fn catalog_contains_all_four_roles() {
    let catalog = catalog();
    assert_eq!(catalog.len(), 4, "expected exactly four roles in catalog");
    for role in [
        SubagentRole::Explorer,
        SubagentRole::Worker,
        SubagentRole::Planner,
        SubagentRole::Reviewer,
    ] {
        let config = role_config(role);
        assert_eq!(config.role, role);
        assert!(
            !config.instructions.trim().is_empty(),
            "role {} should have non-empty instructions",
            role.as_str()
        );
        assert!(
            !config.allowed_tools.is_empty(),
            "role {} should advertise tools",
            role.as_str()
        );
    }
}

#[test]
fn active_roles_are_explorer_planner_reviewer() {
    let active: Vec<_> = catalog()
        .values()
        .filter(|cfg| cfg.status == RoleStatus::Active)
        .map(|cfg| cfg.role)
        .collect();
    assert!(active.contains(&SubagentRole::Explorer));
    assert!(active.contains(&SubagentRole::Planner));
    assert!(active.contains(&SubagentRole::Reviewer));
    assert!(!active.contains(&SubagentRole::Worker));
}

#[test]
fn worker_is_roadmap() {
    assert_eq!(
        role_config(SubagentRole::Worker).status,
        RoleStatus::Roadmap
    );
}

#[test]
fn no_role_advertises_subagent_control_tools() {
    // Flat spawning invariant: subagents must never see delegate/explore/
    // delegate_plan/delegate_review in their advertised tool set, or one
    // subagent could spawn another and we'd lose the cost/cancellation
    // guarantees the parent depends on.
    for cfg in catalog().values() {
        for tool in cfg.allowed_tools {
            assert!(
                !CONTROL_TOOL_NAMES.contains(tool),
                "role {} advertises control tool {tool}",
                cfg.role.as_str()
            );
        }
    }
}

#[test]
fn reviewer_and_planner_are_read_only() {
    // Reviewer/Planner must not be able to mutate the working tree or run
    // shell commands. apply_patch, write_file, and shell are forbidden.
    let mutating = ["apply_patch", "write_file", "shell"];
    for role in [SubagentRole::Reviewer, SubagentRole::Planner] {
        let cfg = role_config(role);
        for tool in cfg.allowed_tools {
            assert!(
                !mutating.contains(tool),
                "role {} must not include mutating tool {tool}",
                role.as_str()
            );
        }
    }
}

#[test]
fn from_str_round_trips_known_roles() {
    for role in [
        SubagentRole::Explorer,
        SubagentRole::Worker,
        SubagentRole::Planner,
        SubagentRole::Reviewer,
    ] {
        assert_eq!(SubagentRole::from_str(role.as_str()), Some(role));
    }
    assert_eq!(SubagentRole::from_str("nonsense"), None);
}
