use super::*;

const CONTROL_TOOL_NAMES: &[&str] = &["delegate", "explore", "delegate_plan", "delegate_review"];

// Prefixes that match the spawn-tool verbs. A tool named `delegate_research`
// or `explore_deep` would slip past an exact-name denylist while still acting
// as a spawn primitive, so we reject the whole prefix family.
const SPAWN_TOOL_PREFIXES: &[&str] = &["delegate_", "explore_", "plan_", "review_"];

// Known graph/planning tools that legitimately share a spawn-verb prefix but
// are not spawn primitives. Anything outside this allow-list that matches a
// `SPAWN_TOOL_PREFIXES` entry is treated as a spawn-tool leak.
const SPAWN_PREFIX_LEGITIMATE: &[&str] = &["plan_patch"];

// Tool names whose substring identifies them as subagent control surface.
const SPAWN_SUBSTRINGS: &[&str] = &["subagent"];

#[test]
fn catalog_contains_all_three_roles() {
    let catalog = catalog();
    assert_eq!(catalog.len(), 3, "expected exactly three roles in catalog");
    for role in [
        SubagentRole::Explorer,
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
fn no_role_advertises_subagent_control_tools() {
    // Flat spawning invariant: subagents must never see delegate/explore/
    // delegate_plan/delegate_review in their advertised tool set, or one
    // subagent could spawn another and we'd lose the cost/cancellation
    // guarantees the parent depends on.
    //
    // Exact-name matching is not enough — a new spawn tool named
    // `delegate_research` or `review_subagent` would silently leak past a
    // literal-only allowlist while restoring hierarchical spawn. Reject by
    // exact name, by spawn-verb prefix family, and by any substring that
    // identifies the tool as subagent control surface.
    for cfg in catalog().values() {
        for tool in cfg.allowed_tools {
            assert!(
                !CONTROL_TOOL_NAMES.contains(tool),
                "role {} advertises control tool {tool}",
                cfg.role.as_str()
            );

            for prefix in SPAWN_TOOL_PREFIXES {
                if tool.starts_with(prefix) && !SPAWN_PREFIX_LEGITIMATE.contains(tool) {
                    panic!(
                        "role {} advertises spawn-shaped tool {tool} matching prefix {prefix:?}; \
                         add it to SPAWN_PREFIX_LEGITIMATE if it is a known non-spawn tool",
                        cfg.role.as_str()
                    );
                }
            }

            for needle in SPAWN_SUBSTRINGS {
                assert!(
                    !tool.contains(needle),
                    "role {} advertises tool {tool} whose name contains {needle:?}, \
                     which marks subagent control surface",
                    cfg.role.as_str()
                );
            }
        }
    }
}

fn is_spawn_shaped(tool: &str) -> bool {
    if CONTROL_TOOL_NAMES.contains(&tool) {
        return true;
    }
    if SPAWN_SUBSTRINGS.iter().any(|needle| tool.contains(needle)) {
        return true;
    }
    SPAWN_TOOL_PREFIXES
        .iter()
        .any(|prefix| tool.starts_with(prefix))
        && !SPAWN_PREFIX_LEGITIMATE.contains(&tool)
}

#[test]
fn flat_spawn_matcher_catches_drifted_tool_names() {
    // Smoke-test the matcher used by `no_role_advertises_subagent_control_tools`
    // so a refactor of the constants cannot silently weaken the invariant.
    // If someone introduces a hypothetical `delegate_research` or `plan_subagent`,
    // these checks must trip the matcher even though the literal allowlist
    // would not.
    for spawn_shaped in [
        // exact spawn tool names
        "delegate",
        "explore",
        "delegate_plan",
        "delegate_review",
        // prefix drift the existing literal allowlist misses
        "delegate_research",
        "delegate_worker",
        "explore_deep",
        "explore_repo",
        "review_subagent",
        "plan_subagent",
        // substring "subagent" anywhere in the tool name
        "spawn_subagent",
        "subagent_control",
    ] {
        assert!(
            is_spawn_shaped(spawn_shaped),
            "matcher must flag {spawn_shaped:?} as spawn-shaped"
        );
    }

    for non_spawn in [
        "read_file",
        "grep",
        "glob",
        "repo_map",
        "plan_patch",
        "shell",
        "apply_patch",
        "diff_context",
        "symbol_context",
    ] {
        assert!(
            !is_spawn_shaped(non_spawn),
            "matcher must not flag legitimate tool {non_spawn:?} as spawn-shaped"
        );
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
        SubagentRole::Planner,
        SubagentRole::Reviewer,
    ] {
        assert_eq!(SubagentRole::from_str(role.as_str()), Some(role));
    }
    assert_eq!(SubagentRole::from_str("worker"), None);
    assert_eq!(SubagentRole::from_str("nonsense"), None);
}
