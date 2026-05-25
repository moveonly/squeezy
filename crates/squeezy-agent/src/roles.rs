use std::{collections::BTreeMap, sync::LazyLock};

use squeezy_core::ReasoningEffort;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SubagentRole {
    Explorer,
    Worker,
    Planner,
    Reviewer,
}

impl SubagentRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Explorer => "explorer",
            Self::Worker => "worker",
            Self::Planner => "planner",
            Self::Reviewer => "reviewer",
        }
    }

    #[cfg(test)]
    pub fn from_str(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "explorer" => Some(Self::Explorer),
            "worker" => Some(Self::Worker),
            "planner" => Some(Self::Planner),
            "reviewer" => Some(Self::Reviewer),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleStatus {
    Active,
    Roadmap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleModelPolicy {
    Parent,
    Cheap,
}

// `description` and `reasoning_effort` are part of the static role catalog
// even though no runtime code reads them yet — they document the role and
// will drive provider-side request shaping once we expose a configuration
// surface for it. `#[allow(dead_code)]` keeps the catalog complete without
// triggering dead-code warnings on the unused fields.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RoleConfig {
    pub role: SubagentRole,
    pub status: RoleStatus,
    pub description: &'static str,
    pub instructions: &'static str,
    pub allowed_tools: &'static [&'static str],
    pub model_policy: RoleModelPolicy,
    pub reasoning_effort: Option<ReasoningEffort>,
}

const EXPLORER_TOOLS: &[&str] = &[
    "repo_map",
    "decl_search",
    "definition_search",
    "reference_search",
    "upstream_flow",
    "downstream_flow",
    "hierarchy",
    "symbol_context",
    "read_slice",
    "read_file",
    "grep",
    "glob",
];

const WORKER_TOOLS: &[&str] = &[
    "repo_map",
    "decl_search",
    "definition_search",
    "reference_search",
    "upstream_flow",
    "downstream_flow",
    "hierarchy",
    "symbol_context",
    "read_slice",
    "read_file",
    "grep",
    "glob",
    "apply_patch",
    "write_file",
    "shell",
    "checkpoint_list",
    "checkpoint_show",
    "checkpoint_undo",
    "checkpoint_revert",
    "plan_patch",
    "diff_context",
];

const PLANNER_TOOLS: &[&str] = &[
    "repo_map",
    "plan_patch",
    "decl_search",
    "definition_search",
    "upstream_flow",
    "downstream_flow",
    "read_slice",
    "read_file",
    "glob",
    "grep",
];

const REVIEWER_TOOLS: &[&str] = &[
    "diff_context",
    "read_slice",
    "read_file",
    "decl_search",
    "reference_search",
    "symbol_context",
    "glob",
    "grep",
];

pub fn catalog() -> &'static BTreeMap<&'static str, RoleConfig> {
    static CATALOG: LazyLock<BTreeMap<&'static str, RoleConfig>> = LazyLock::new(|| {
        BTreeMap::from([
            (
                "explorer",
                RoleConfig {
                    role: SubagentRole::Explorer,
                    status: RoleStatus::Active,
                    description: "Graph-first codebase exploration.",
                    instructions: "Use semantic graph tools first. Use glob, grep, and read_file only as bounded fallback. If graph searches return zero matches, switch to path/file discovery rather than repeating equivalent declaration searches. Return a compact briefing with relevant files, symbols, risks, and minimum next actions.",
                    allowed_tools: EXPLORER_TOOLS,
                    model_policy: RoleModelPolicy::Cheap,
                    reasoning_effort: Some(ReasoningEffort::Low),
                },
            ),
            (
                "worker",
                RoleConfig {
                    role: SubagentRole::Worker,
                    status: RoleStatus::Roadmap,
                    description: "Future mutation-capable worker role.",
                    instructions: "Own an explicitly assigned implementation area and do not revert unrelated edits.",
                    allowed_tools: WORKER_TOOLS,
                    model_policy: RoleModelPolicy::Parent,
                    reasoning_effort: Some(ReasoningEffort::Medium),
                },
            ),
            (
                "planner",
                RoleConfig {
                    role: SubagentRole::Planner,
                    status: RoleStatus::Active,
                    description: "Read-only graph-backed implementation planning.",
                    instructions: "Build an implementation plan from graph evidence. Use plan_patch when an edit target is known so the parent receives a persisted plan_id and impacted neighborhood. Do not mutate files or run shell commands.",
                    allowed_tools: PLANNER_TOOLS,
                    model_policy: RoleModelPolicy::Parent,
                    reasoning_effort: Some(ReasoningEffort::High),
                },
            ),
            (
                "reviewer",
                RoleConfig {
                    role: SubagentRole::Reviewer,
                    status: RoleStatus::Active,
                    description: "Read-only review of changed code.",
                    instructions: "Review the current diff with diff_context and graph-backed reads. Report only actionable issues with severity, file, line, message, and suggested fix. Return pass=true when no blocker or warning remains.",
                    allowed_tools: REVIEWER_TOOLS,
                    model_policy: RoleModelPolicy::Cheap,
                    reasoning_effort: Some(ReasoningEffort::Low),
                },
            ),
        ])
    });
    &CATALOG
}

pub fn role_config(role: SubagentRole) -> &'static RoleConfig {
    catalog()
        .get(role.as_str())
        .expect("built-in subagent role must exist")
}

#[cfg(test)]
#[path = "roles_tests.rs"]
mod tests;
