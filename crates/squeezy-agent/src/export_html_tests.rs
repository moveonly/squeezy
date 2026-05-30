use super::*;
use serde_json::json;
use squeezy_core::SessionMode;
use squeezy_store::{
    SESSION_METADATA_SCHEMA_VERSION, SessionEvent, SessionMetadata, SessionRecord, SessionStatus,
};

fn meta(id: &str) -> SessionMetadata {
    SessionMetadata {
        schema_version: SESSION_METADATA_SCHEMA_VERSION,
        session_id: id.to_string(),
        started_at_ms: 1_700_000_000_000,
        ended_at_ms: Some(1_700_000_120_000),
        archived_at_ms: None,
        cwd: "/tmp/work".to_string(),
        workspace_root: "/tmp/work".to_string(),
        repo_root: Some("/tmp/work".to_string()),
        branch: Some("main".to_string()),
        provider: "openai".to_string(),
        model: "test-model".to_string(),
        mode: SessionMode::Build,
        status: SessionStatus::Completed,
        first_user_task: Some("hello".to_string()),
        latest_summary: None,
        cost: Default::default(),
        metrics: Default::default(),
        redactions: 0,
        resume_available: false,
        resume_unavailable_reason: None,
        event_count: 0,
        token_calibration: Default::default(),
        parent_id: None,
        display_name: None,
        labels: Vec::new(),
    }
}

fn event(kind: &str, summary: Option<&str>, payload: serde_json::Value) -> SessionEvent {
    SessionEvent {
        ts_unix_ms: 1_700_000_000_001,
        kind: kind.to_string(),
        turn_id: None,
        summary: summary.map(str::to_string),
        payload,
        parent_event_sequence: None,
    }
}

fn record(events: Vec<SessionEvent>) -> SessionRecord {
    let event_count = events.len() as u64;
    let mut metadata = meta("session-test");
    metadata.event_count = event_count;
    SessionRecord {
        metadata,
        events,
        event_warnings: 0,
        resume_state: None,
        attachments: Vec::new(),
        replay: None,
    }
}

#[test]
fn html_export_is_self_contained_and_renders_messages() {
    let session = record(vec![
        event(
            "user_message",
            Some("hello there"),
            json!({"text": "hello there"}),
        ),
        event(
            "assistant_completed",
            Some("hi"),
            json!({"text": "hi, world!"}),
        ),
    ]);
    let html = export_session_to_html(&session, &ExportOpts::default()).unwrap();
    assert!(html.starts_with("<!DOCTYPE html>"), "missing doctype");
    assert!(html.contains("<style>"), "missing inline style block");
    // No external resources of any kind.
    assert!(!html.contains("<link"), "must not link external CSS");
    assert!(!html.contains("<script"), "must not contain script tags");
    assert!(!html.contains("href=\"http"), "no external http hrefs");
    assert!(!html.contains("src=\""), "no external src attributes");
    assert!(html.contains("hello there"), "user message missing");
    assert!(html.contains("hi, world!"), "assistant message missing");
    assert!(
        html.contains("session-test"),
        "session id missing from header"
    );
}

#[test]
fn xss_user_message_is_escaped() {
    let payload = "<script>alert('xss')</script>";
    let session = record(vec![event(
        "user_message",
        Some(payload),
        json!({"text": payload}),
    )]);
    let html = export_session_to_html(&session, &ExportOpts::default()).unwrap();
    // The literal `<script>` substring must never appear as raw markup
    // (escape guard); the escaped form does.
    assert!(
        !html.contains("<script>alert"),
        "user input escaped into raw markup: {html}"
    );
    assert!(
        html.contains("&lt;script&gt;alert(&#x27;xss&#x27;)&lt;/script&gt;"),
        "expected escaped payload, got: {html}"
    );
}

#[test]
fn xss_tool_arguments_and_output_are_escaped() {
    let tool_call = event(
        "tool_call",
        None,
        json!({
            "call_id": "call-1",
            "tool": "bash",
            "arguments": {"cmd": "<img src=x onerror=alert(1)>"},
        }),
    );
    let tool_result = event(
        "tool_result",
        None,
        json!({
            "output": {
                "call_id": "call-1",
                "output": "<script>steal()</script>",
            }
        }),
    );
    let session = record(vec![tool_call, tool_result]);
    let html = export_session_to_html(&session, &ExportOpts::default()).unwrap();
    assert!(
        !html.contains("<img src=x"),
        "tool argument escaped into raw markup"
    );
    assert!(
        !html.contains("<script>steal()"),
        "tool output escaped into raw markup"
    );
    assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
    assert!(html.contains("&lt;script&gt;steal()&lt;/script&gt;"));
}

#[test]
fn ansi_color_codes_become_inline_spans() {
    // ESC[31m red ESC[0m
    let ansi_text = "\x1b[31mhello\x1b[0m world";
    let tool_result = event(
        "tool_result",
        None,
        json!({
            "output": {
                "call_id": "call-1",
                "output": ansi_text,
            }
        }),
    );
    let session = record(vec![tool_result]);
    let html = export_session_to_html(&session, &ExportOpts::default()).unwrap();
    assert!(
        html.contains("<span style=\"color:#800000\">hello</span> world"),
        "ansi to html mapping failed: {html}"
    );
    // No raw ESC bytes escape into the document.
    assert!(!html.contains('\x1b'), "raw ESC bytes leaked into document");
}

#[test]
fn ansi_to_html_handles_256_and_rgb() {
    // 256-color foreground
    let html_256 = ansi_to_html("\x1b[38;5;196mred256\x1b[0m");
    assert!(
        html_256.contains("color:#"),
        "256-color did not produce inline color: {html_256}"
    );
    assert!(html_256.contains("red256"));
    // RGB true color
    let html_rgb = ansi_to_html("\x1b[38;2;10;20;30mrgb\x1b[0m");
    assert!(
        html_rgb.contains("color:#0a141e") || html_rgb.contains("color:#0A141E"),
        "rgb did not produce expected hex: {html_rgb}"
    );
}

#[test]
fn ansi_to_html_escapes_html_inside_styled_runs() {
    // The `<` should appear escaped *inside* the styled span.
    let html = ansi_to_html("\x1b[1m<b>danger</b>\x1b[0m");
    assert!(html.contains("&lt;b&gt;danger&lt;/b&gt;"));
    assert!(!html.contains("<b>danger</b>"));
}

#[test]
fn whitespace_in_tool_output_is_preserved() {
    let body = "line1\n  indented\nlast";
    let tool_result = event(
        "tool_result",
        None,
        json!({
            "output": {
                "call_id": "call-1",
                "output": body,
            }
        }),
    );
    let session = record(vec![tool_result]);
    let html = export_session_to_html(&session, &ExportOpts::default()).unwrap();
    // Each line wrapped in an ansi-line div, indentation kept.
    assert!(html.contains("<div class=\"ansi-line\">line1</div>"));
    assert!(html.contains("<div class=\"ansi-line\">  indented</div>"));
    assert!(html.contains("<div class=\"ansi-line\">last</div>"));
    // CSS keeps `white-space: pre` on .ansi-line so indentation renders.
    assert!(html.contains(".ansi-line"));
    assert!(html.contains("white-space:pre"));
}

#[test]
fn include_tool_outputs_false_drops_tool_events() {
    let session = record(vec![
        event("user_message", Some("run ls"), json!({"text": "run ls"})),
        event(
            "tool_call",
            None,
            json!({
                "call_id": "call-1",
                "tool": "ls",
                "arguments": {"path": "/tmp"},
            }),
        ),
        event(
            "tool_result",
            None,
            json!({
                "output": {
                    "call_id": "call-1",
                    "output": "a\nb\nc",
                }
            }),
        ),
    ]);
    let html = export_session_to_html(
        &session,
        &ExportOpts {
            include_tool_outputs: false,
            theme: ExportTheme::Light,
        },
    )
    .unwrap();
    assert!(html.contains("run ls"));
    // The class names show up unconditionally in the bundled CSS rule
    // set; the rendered element is what we want to verify is gone.
    assert!(!html.contains("<li class=\"msg msg-tool\">"));
    assert!(!html.contains("Tool: ls"));
    assert!(!html.contains("<div class=\"ansi-line\">"));
}

#[test]
fn theme_choice_changes_body_class() {
    let session = record(vec![]);
    let dark = export_session_to_html(
        &session,
        &ExportOpts {
            include_tool_outputs: true,
            theme: ExportTheme::Dark,
        },
    )
    .unwrap();
    let light = export_session_to_html(
        &session,
        &ExportOpts {
            include_tool_outputs: true,
            theme: ExportTheme::Light,
        },
    )
    .unwrap();
    assert!(dark.contains("class=\"theme-dark\""));
    assert!(light.contains("class=\"theme-light\""));
}

#[test]
fn lifecycle_events_render_compactly() {
    let session = record(vec![
        event("session_started", None, json!({})),
        event(
            "session_ended",
            Some("completed"),
            json!({"status": "completed"}),
        ),
    ]);
    let html = export_session_to_html(&session, &ExportOpts::default()).unwrap();
    // We deliberately drop the "session_started" event from the
    // rendered output (it adds noise without information) but the
    // ended event has a useful status.
    assert!(html.contains("session ended"));
    assert!(html.contains("completed"));
}

#[test]
fn tool_call_without_result_still_renders() {
    let session = record(vec![event(
        "tool_call",
        None,
        json!({
            "call_id": "call-1",
            "tool": "bash",
            "arguments": {"cmd": "echo hi"},
        }),
    )]);
    let html = export_session_to_html(&session, &ExportOpts::default()).unwrap();
    assert!(html.contains("Tool: bash"));
    assert!(html.contains("echo hi"));
}
