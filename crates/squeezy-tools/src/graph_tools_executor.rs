use serde_json::json;

use crate::{ToolCall, ToolCostHint, ToolRegistry, ToolResult, ToolStatus, make_result};

pub(crate) fn is_graph_tool_name(name: &str) -> bool {
    matches!(
        name,
        "repo_map"
            | "decl_search"
            | "definition_search"
            | "reference_search"
            | "upstream_flow"
            | "downstream_flow"
            | "symbol_context"
            | "hierarchy"
            | "inheritance_hierarchy"
            | "impact"
            | "symbol_at"
            | "read_slice"
    )
}

impl ToolRegistry {
    pub(crate) fn is_graph_tool_name(name: &str) -> bool {
        is_graph_tool_name(name)
    }

    pub(crate) async fn execute_graph_tool(&self, call: &ToolCall) -> ToolResult {
        let registry = self.clone();
        let call = call.clone();
        let fallback_call = call.clone();
        tokio::task::spawn_blocking(move || registry.execute_graph_tool_blocking(&call))
            .await
            .unwrap_or_else(|err| {
                make_result(
                    &fallback_call,
                    ToolStatus::Error,
                    json!({ "error": format!("graph tool join failed: {err}") }),
                    ToolCostHint::default(),
                    None,
                )
            })
    }
}
