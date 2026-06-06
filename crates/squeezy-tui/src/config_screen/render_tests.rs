use super::*;

/// Char index of the caret (the first span carrying `Modifier::REVERSED`)
/// within the joined text of a line, counting chars in the spans before it.
/// Returns `None` when no span is reversed.
fn caret_char_index(line: &Line<'_>) -> Option<usize> {
    let mut offset = 0usize;
    for span in &line.spans {
        if span.style.add_modifier.contains(Modifier::REVERSED) {
            return Some(offset);
        }
        offset += span.content.chars().count();
    }
    None
}

fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[test]
fn caret_line_marks_cursor_mid_string_not_end() {
    // Two leading indent spaces then "hello"; cursor parked on index 2 ('l').
    let line = caret_line("hello", 2);
    // Indent is two chars, so the caret should land at joined index 4.
    assert_eq!(caret_char_index(&line), Some(4));
    assert_eq!(line_text(&line), "  hello");
}

#[test]
fn caret_line_at_end_reverses_trailing_space() {
    let line = caret_line("hi", 2);
    // Caret sits just past the last char: joined index 4 (2 indent + "hi").
    assert_eq!(caret_char_index(&line), Some(4));
    // A trailing space is drawn so the insertion point is visible.
    assert_eq!(line_text(&line), "  hi ");
}

#[test]
fn caret_line_handles_multibyte_chars() {
    // Cursor at char index 1 must split on a char boundary, not a byte one.
    let line = caret_line("café", 1);
    assert_eq!(caret_char_index(&line), Some(3));
    assert_eq!(line_text(&line), "  café");
}

#[test]
fn caret_line_clamps_out_of_range_cursor() {
    // Stale cursor past the end must not panic and parks on a trailing space.
    let line = caret_line("ab", 99);
    assert_eq!(line_text(&line), "  ab ");
    assert_eq!(caret_char_index(&line), Some(4));
}

#[test]
fn secret_caret_line_marks_cursor_mid_string() {
    // Masked display of a 5-char key, cursor on index 2.
    let line = secret_caret_line("•••••", 2);
    assert_eq!(caret_char_index(&line), Some(4));
}

#[test]
fn secret_caret_line_at_end_uses_underscore() {
    let line = secret_caret_line("•••", 3);
    // No reversed span when parked past the end; an accent underscore marks it.
    assert_eq!(caret_char_index(&line), None);
    assert_eq!(line_text(&line), "  •••_");
}

#[test]
fn mcp_status_icon_picks_glyph_per_server_state() {
    use squeezy_core::{McpPermissionConfig, McpServerConfig, McpTransport};
    use squeezy_tools::McpServerStatus;
    fn server(enabled: bool) -> McpServerConfig {
        McpServerConfig {
            enabled,
            transport: McpTransport::Stdio,
            command: Some("x".to_string()),
            args: Vec::new(),
            url: None,
            timeout_ms: None,
            discovery_timeout_ms: None,
            tool_call_timeout_ms: None,
            enabled_tools: None,
            disabled_tools: Vec::new(),
            env: std::collections::BTreeMap::new(),
            permissions: McpPermissionConfig::default(),
            bearer_token_env_var: None,
            http_headers: std::collections::BTreeMap::new(),
            env_http_headers: std::collections::BTreeMap::new(),
        }
    }

    let enabled = server(true);
    let disabled = server(false);

    // Disabled wins over any stale snapshot — even if discovery
    // left a `Ready` row behind, the row must read as muted /
    // silver so the user can tell at a glance the server is off.
    let ready = McpServerStatus::Ready {
        tools_count: 3,
        cached: false,
    };
    let (icon, _) = mcp_status_icon(&disabled, Some(&ready), 0);
    assert_eq!(icon, '●', "disabled servers render a filled circle");

    // Ready (fresh) = filled circle in the success palette.
    let (icon, color) = mcp_status_icon(&enabled, Some(&ready), 0);
    assert_eq!(icon, '●');
    assert_eq!(color, crate::render::theme::green());

    // Ready (cached) gets a distinct accent so a stale palette is
    // visually different from a freshly-discovered one.
    let cached = McpServerStatus::Ready {
        tools_count: 3,
        cached: true,
    };
    let (icon, color) = mcp_status_icon(&enabled, Some(&cached), 0);
    assert_eq!(icon, '●');
    assert_eq!(color, crate::render::theme::cyan());

    let stale = McpServerStatus::Stale {
        tools_count: 3,
        outcome: squeezy_tools::McpStaleOutcome::Failed {
            error: "boom".to_string(),
        },
    };
    let (icon, color) = mcp_status_icon(&enabled, Some(&stale), 0);
    assert_eq!(icon, '●');
    assert_eq!(color, crate::render::theme::cyan());

    // Failure modes all read red so a busted server cannot
    // accidentally look ready.
    for status in [
        McpServerStatus::Failed {
            error: "boom".to_string(),
        },
        McpServerStatus::Cancelled,
    ] {
        let (icon, color) = mcp_status_icon(&enabled, Some(&status), 0);
        assert_eq!(icon, '●');
        assert_eq!(color, crate::render::theme::red());
    }

    // Starting blinks slowly between open and filled circles instead of
    // spinning every frame.
    let starting = McpServerStatus::Starting;
    let frames: Vec<char> = [0, 9, 10, 19, 20]
        .into_iter()
        .map(|tick| mcp_status_icon(&enabled, Some(&starting), tick).0)
        .collect();
    assert_eq!(frames, vec!['○', '○', '●', '●', '○']);
    assert_eq!(
        mcp_status_icon(&disabled, Some(&starting), 10).0,
        '●',
        "pending state must blink even while the server is toggling off"
    );

    // Unknown (enabled server with no snapshot yet) reads as an
    // open circle so it isn't confused with ready / failed.
    assert_eq!(mcp_status_icon(&enabled, None, 0).0, '○');
}

#[test]
fn mcp_status_text_distinguishes_stopping_from_starting() {
    use squeezy_core::{McpPermissionConfig, McpServerConfig, McpTransport};

    let mut server = McpServerConfig {
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("x".to_string()),
        args: Vec::new(),
        url: None,
        timeout_ms: None,
        discovery_timeout_ms: None,
        tool_call_timeout_ms: None,
        enabled_tools: None,
        disabled_tools: Vec::new(),
        env: std::collections::BTreeMap::new(),
        permissions: McpPermissionConfig::default(),
        bearer_token_env_var: None,
        http_headers: std::collections::BTreeMap::new(),
        env_http_headers: std::collections::BTreeMap::new(),
    };
    assert_eq!(
        format_mcp_row_status_for_server(&server, &squeezy_tools::McpServerStatus::Starting),
        "starting"
    );
    server.enabled = false;
    assert_eq!(
        format_mcp_row_status_for_server(&server, &squeezy_tools::McpServerStatus::Starting),
        "stopping"
    );
}

#[test]
fn mcp_status_cell_keeps_actionable_error_prefix() {
    let failed = squeezy_tools::McpServerStatus::Failed {
        error: "command docs-mcp not found on PATH after restart".to_string(),
    };
    let text = format_mcp_row_status(&failed);
    assert_eq!(
        text,
        "failed: command docs-mcp not found on PATH after restart"
    );

    let cell = mcp_status_cell(&text, MCP_STATUS_COLUMN_WIDTH);
    assert_eq!(cell, "failed: command docs-mcp not foun...");
    assert!(cell.chars().count() <= MCP_STATUS_COLUMN_WIDTH);

    let stale = squeezy_tools::McpServerStatus::Stale {
        tools_count: 2,
        outcome: squeezy_tools::McpStaleOutcome::Failed {
            error: "connection refused".to_string(),
        },
    };
    assert_eq!(
        format_mcp_row_status(&stale),
        "stale: connection refused (2 cached)"
    );
}
