use squeezy_core::{PermissionCapability, PermissionScope};

use crate::schema::compact_typed_tool_parameters;
use crate::specs::{
    apply_patch_spec, checkpoint_check_spec, checkpoint_doctor_spec, checkpoint_list_spec,
    checkpoint_restore_file_spec, checkpoint_revert_spec, checkpoint_show_spec,
    checkpoint_undo_spec, decl_search_spec, definition_search_spec, diff_context_spec,
    downstream_flow_spec, glob_spec, grep_spec, hierarchy_spec, impact_spec,
    inheritance_hierarchy_spec, list_skills_spec, load_skill_spec,
    mcp_list_resource_templates_spec, mcp_list_resources_spec, mcp_read_resource_spec,
    mcp_tool_spec, memory_spec, notebook_edit_spec, notes_recall_spec, notes_remember_spec,
    observations_spec, plan_patch_spec, prepare_path_arguments, read_file_spec, read_slice_spec,
    read_tool_output_spec, reference_search_spec, refresh_compiler_facts_spec, repo_map_spec,
    shell_spec, symbol_at_spec, symbol_context_spec, upstream_flow_spec, verify_spec,
    webfetch_spec, websearch_spec, write_file_spec,
};
use crate::{
    McpServerStatus, PrepareArgumentsHook, ToolCall, ToolRegistry, ToolSpec, grep_include_ignored,
    tool_include_ignored,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FirstPartyToolExecutor {
    ApplyPatch,
    CheckpointDoctor,
    CheckpointList,
    CheckpointShow,
    CheckpointUndo,
    CheckpointRevert,
    CheckpointRestoreFile,
    CheckpointCheck,
    Graph,
    DiffContext,
    PlanPatch,
    Glob,
    Grep,
    ReadFile,
    ReadToolOutput,
    RefreshCompilerFacts,
    Verify,
    NotebookEdit,
    WriteFile,
    Shell,
    Webfetch,
    Websearch,
    McpListResources,
    McpListResourceTemplates,
    McpReadResource,
    ListSkills,
    LoadSkill,
    NotesRemember,
    NotesRecall,
    Observations,
    Memory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PermissionProfile {
    ApplyPatch,
    CheckpointMutation,
    WriteFile,
    ReadFile,
    Shell,
    Verify,
    RefreshCompilerFacts,
    Webfetch,
    Websearch,
    McpReadResource,
    McpListResources,
    IgnoredGlob,
    IgnoredGrep,
    GraphSearch,
    WorkspaceSearch,
    WorkspaceRead,
    DefaultRead,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ToolDescriptor {
    pub(crate) name: &'static str,
    pub(crate) executor: FirstPartyToolExecutor,
    pub(crate) permission_profile: PermissionProfile,
    pub(crate) scope: PermissionScope,
}

impl ToolDescriptor {
    pub(crate) const fn new(
        name: &'static str,
        executor: FirstPartyToolExecutor,
        permission_profile: PermissionProfile,
        scope: PermissionScope,
    ) -> Self {
        Self {
            name,
            executor,
            permission_profile,
            scope,
        }
    }
}

const FIRST_PARTY_DESCRIPTORS: &[ToolDescriptor] = &[
    ToolDescriptor::new(
        "apply_patch",
        FirstPartyToolExecutor::ApplyPatch,
        PermissionProfile::ApplyPatch,
        PermissionScope::Edit,
    ),
    ToolDescriptor::new(
        "checkpoint_doctor",
        FirstPartyToolExecutor::CheckpointDoctor,
        PermissionProfile::WorkspaceRead,
        PermissionScope::Read,
    ),
    ToolDescriptor::new(
        "checkpoint_list",
        FirstPartyToolExecutor::CheckpointList,
        PermissionProfile::WorkspaceRead,
        PermissionScope::Read,
    ),
    ToolDescriptor::new(
        "checkpoint_show",
        FirstPartyToolExecutor::CheckpointShow,
        PermissionProfile::WorkspaceRead,
        PermissionScope::Read,
    ),
    ToolDescriptor::new(
        "checkpoint_undo",
        FirstPartyToolExecutor::CheckpointUndo,
        PermissionProfile::CheckpointMutation,
        PermissionScope::Edit,
    ),
    ToolDescriptor::new(
        "checkpoint_revert",
        FirstPartyToolExecutor::CheckpointRevert,
        PermissionProfile::CheckpointMutation,
        PermissionScope::Edit,
    ),
    ToolDescriptor::new(
        "checkpoint_restore_file",
        FirstPartyToolExecutor::CheckpointRestoreFile,
        PermissionProfile::CheckpointMutation,
        PermissionScope::Edit,
    ),
    ToolDescriptor::new(
        "checkpoint_check",
        FirstPartyToolExecutor::CheckpointCheck,
        PermissionProfile::WorkspaceRead,
        PermissionScope::Read,
    ),
    ToolDescriptor::new(
        "diff_context",
        FirstPartyToolExecutor::DiffContext,
        PermissionProfile::WorkspaceRead,
        PermissionScope::Read,
    ),
    ToolDescriptor::new(
        "plan_patch",
        FirstPartyToolExecutor::PlanPatch,
        PermissionProfile::WorkspaceRead,
        PermissionScope::Read,
    ),
    ToolDescriptor::new(
        "glob",
        FirstPartyToolExecutor::Glob,
        PermissionProfile::WorkspaceSearch,
        PermissionScope::Read,
    ),
    ToolDescriptor::new(
        "grep",
        FirstPartyToolExecutor::Grep,
        PermissionProfile::WorkspaceSearch,
        PermissionScope::Read,
    ),
    ToolDescriptor::new(
        "read_file",
        FirstPartyToolExecutor::ReadFile,
        PermissionProfile::ReadFile,
        PermissionScope::Read,
    ),
    ToolDescriptor::new(
        "read_tool_output",
        FirstPartyToolExecutor::ReadToolOutput,
        PermissionProfile::WorkspaceRead,
        PermissionScope::Read,
    ),
    ToolDescriptor::new(
        "refresh_compiler_facts",
        FirstPartyToolExecutor::RefreshCompilerFacts,
        PermissionProfile::RefreshCompilerFacts,
        PermissionScope::Shell,
    ),
    ToolDescriptor::new(
        "verify",
        FirstPartyToolExecutor::Verify,
        PermissionProfile::Verify,
        PermissionScope::Shell,
    ),
    ToolDescriptor::new(
        "notebook_edit",
        FirstPartyToolExecutor::NotebookEdit,
        PermissionProfile::WriteFile,
        PermissionScope::Edit,
    ),
    ToolDescriptor::new(
        "write_file",
        FirstPartyToolExecutor::WriteFile,
        PermissionProfile::WriteFile,
        PermissionScope::Edit,
    ),
    ToolDescriptor::new(
        "shell",
        FirstPartyToolExecutor::Shell,
        PermissionProfile::Shell,
        PermissionScope::Shell,
    ),
    ToolDescriptor::new(
        "webfetch",
        FirstPartyToolExecutor::Webfetch,
        PermissionProfile::Webfetch,
        PermissionScope::Web,
    ),
    ToolDescriptor::new(
        "websearch",
        FirstPartyToolExecutor::Websearch,
        PermissionProfile::Websearch,
        PermissionScope::Web,
    ),
    ToolDescriptor::new(
        "mcp_list_resources",
        FirstPartyToolExecutor::McpListResources,
        PermissionProfile::McpListResources,
        PermissionScope::Mcp,
    ),
    ToolDescriptor::new(
        "mcp_list_resource_templates",
        FirstPartyToolExecutor::McpListResourceTemplates,
        PermissionProfile::McpListResources,
        PermissionScope::Mcp,
    ),
    ToolDescriptor::new(
        "mcp_read_resource",
        FirstPartyToolExecutor::McpReadResource,
        PermissionProfile::McpReadResource,
        PermissionScope::Mcp,
    ),
    ToolDescriptor::new(
        "list_skills",
        FirstPartyToolExecutor::ListSkills,
        PermissionProfile::WorkspaceRead,
        PermissionScope::Read,
    ),
    ToolDescriptor::new(
        "load_skill",
        FirstPartyToolExecutor::LoadSkill,
        PermissionProfile::WorkspaceRead,
        PermissionScope::Read,
    ),
    ToolDescriptor::new(
        "notes_remember",
        FirstPartyToolExecutor::NotesRemember,
        PermissionProfile::DefaultRead,
        PermissionScope::Read,
    ),
    ToolDescriptor::new(
        "notes_recall",
        FirstPartyToolExecutor::NotesRecall,
        PermissionProfile::DefaultRead,
        PermissionScope::Read,
    ),
    ToolDescriptor::new(
        "observations",
        FirstPartyToolExecutor::Observations,
        PermissionProfile::WorkspaceRead,
        PermissionScope::Read,
    ),
    ToolDescriptor::new(
        "memory",
        FirstPartyToolExecutor::Memory,
        PermissionProfile::DefaultRead,
        PermissionScope::Read,
    ),
];

pub(crate) fn descriptor(name: &str) -> Option<ToolDescriptor> {
    if ToolRegistry::is_graph_tool_name(name) {
        return Some(ToolDescriptor::new(
            "<graph>",
            FirstPartyToolExecutor::Graph,
            graph_permission_profile(name),
            PermissionScope::Read,
        ));
    }
    FIRST_PARTY_DESCRIPTORS
        .iter()
        .copied()
        .find(|descriptor| descriptor.name == name)
}

pub(crate) fn descriptor_for_call(call: &ToolCall) -> Option<ToolDescriptor> {
    let mut descriptor = descriptor(&call.name)?;
    descriptor.permission_profile = match descriptor.permission_profile {
        PermissionProfile::WorkspaceSearch
            if call.name == "glob" && tool_include_ignored(&call.arguments) =>
        {
            PermissionProfile::IgnoredGlob
        }
        PermissionProfile::WorkspaceSearch
            if call.name == "grep" && grep_include_ignored(&call.arguments) =>
        {
            PermissionProfile::IgnoredGrep
        }
        profile => profile,
    };
    descriptor.scope = match descriptor.permission_profile {
        PermissionProfile::IgnoredGlob | PermissionProfile::IgnoredGrep => {
            PermissionScope::IgnoredSearch
        }
        _ => descriptor.scope,
    };
    Some(descriptor)
}

pub(crate) fn build_specs(registry: &ToolRegistry) -> Vec<ToolSpec> {
    let mut specs = vec![
        apply_patch_spec(),
        decl_search_spec(),
        definition_search_spec(),
        diff_context_spec(),
        downstream_flow_spec(),
        glob_spec(),
        grep_spec(),
        hierarchy_spec(),
        impact_spec(),
        inheritance_hierarchy_spec(),
        notebook_edit_spec(),
        plan_patch_spec(),
        read_file_spec(),
        read_slice_spec(),
        read_tool_output_spec(),
        reference_search_spec(),
        refresh_compiler_facts_spec(),
        repo_map_spec(),
        write_file_spec(),
        symbol_context_spec(),
        upstream_flow_spec(),
        verify_spec(),
        shell_spec(),
        webfetch_spec(),
        websearch_spec(),
        list_skills_spec(),
        load_skill_spec(),
        notes_remember_spec(),
        notes_recall_spec(),
        observations_spec(),
        memory_spec(),
    ];
    if !registry.mcp.has_no_enabled_servers() {
        specs.extend([
            mcp_list_resources_spec(),
            mcp_list_resource_templates_spec(),
            mcp_read_resource_spec(),
        ]);
    }
    specs.push(checkpoint_list_spec());
    if registry.checkpoints.is_some() {
        specs.extend([
            checkpoint_check_spec(),
            checkpoint_doctor_spec(),
            checkpoint_restore_file_spec(),
            checkpoint_revert_spec(),
            checkpoint_show_spec(),
            checkpoint_undo_spec(),
        ]);
    }
    if registry.graph_available_for_specs() {
        specs.push(symbol_at_spec());
    }
    for spec in specs.iter_mut() {
        compact_typed_tool_parameters(&mut spec.parameters);
        if spec.prepare_arguments.is_none() && spec_has_top_level_path(spec) {
            spec.prepare_arguments = Some(prepare_path_arguments);
        }
    }
    let mcp_status = registry.mcp.status_snapshot();
    specs.extend(registry.mcp.tools().into_iter().map(|tool| {
        let is_stale = matches!(
            mcp_status.per_server.get(&tool.server),
            Some(McpServerStatus::Stale { .. })
        );
        mcp_tool_spec(tool, is_stale)
    }));
    specs.sort_by(|left, right| {
        let left_mcp = left.name.starts_with("mcp__");
        let right_mcp = right.name.starts_with("mcp__");
        left_mcp
            .cmp(&right_mcp)
            .then_with(|| left.name.cmp(&right.name))
    });
    specs
}

pub(crate) fn prepare_arguments_for(
    registry: &ToolRegistry,
    name: &str,
) -> Option<PrepareArgumentsHook> {
    registry
        .specs()
        .iter()
        .find(|spec| spec.name == name)
        .and_then(|spec| spec.prepare_arguments)
}

pub(crate) fn default_permission_tuple(
    profile: PermissionProfile,
) -> (PermissionCapability, String, squeezy_core::PermissionRisk) {
    use squeezy_core::PermissionRisk;
    match profile {
        PermissionProfile::GraphSearch => (
            PermissionCapability::Search,
            "workspace:*".to_string(),
            PermissionRisk::Low,
        ),
        PermissionProfile::WorkspaceSearch => (
            PermissionCapability::Search,
            "workspace:*".to_string(),
            PermissionRisk::Low,
        ),
        PermissionProfile::WorkspaceRead => (
            PermissionCapability::Read,
            "workspace:*".to_string(),
            PermissionRisk::Low,
        ),
        PermissionProfile::IgnoredGlob | PermissionProfile::IgnoredGrep => (
            PermissionCapability::Search,
            "ignored:*".to_string(),
            PermissionRisk::Medium,
        ),
        _ => (
            PermissionCapability::Read,
            "tool:*".to_string(),
            PermissionRisk::Medium,
        ),
    }
}

fn graph_permission_profile(name: &str) -> PermissionProfile {
    match name {
        "decl_search" | "definition_search" | "reference_search" => PermissionProfile::GraphSearch,
        "diff_context" | "downstream_flow" | "hierarchy" | "read_slice" | "repo_map"
        | "symbol_context" | "upstream_flow" => PermissionProfile::WorkspaceRead,
        _ => PermissionProfile::DefaultRead,
    }
}

fn spec_has_top_level_path(spec: &ToolSpec) -> bool {
    spec.parameters
        .properties
        .as_ref()
        .is_some_and(|props| props.contains_key("path"))
}
