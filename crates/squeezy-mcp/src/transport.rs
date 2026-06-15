use squeezy_core::McpServerConfig;

pub(crate) const DEFAULT_MCP_TIMEOUT_MS: u64 = 30_000;

pub(crate) fn discovery_timeout_ms(server: &McpServerConfig) -> u64 {
    server
        .discovery_timeout_ms
        .or(server.timeout_ms)
        .unwrap_or(DEFAULT_MCP_TIMEOUT_MS)
}

pub(crate) fn tool_call_timeout_ms(server: &McpServerConfig) -> u64 {
    server
        .tool_call_timeout_ms
        .or(server.timeout_ms)
        .unwrap_or(DEFAULT_MCP_TIMEOUT_MS)
}
