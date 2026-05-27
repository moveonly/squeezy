use std::path::PathBuf;

use serde_json::json;
use squeezy_core::{PermissionCapability, PermissionRequest, PermissionRisk};

use super::*;

fn req(tool: &str) -> PermissionRequest {
    PermissionRequest {
        call_id: "c".to_string(),
        tool_name: tool.to_string(),
        capability: PermissionCapability::Shell,
        target: "?".to_string(),
        risk: PermissionRisk::Medium,
        summary: "summary".to_string(),
        metadata: Default::default(),
        suggested_rules: Vec::new(),
    }
}

#[test]
fn preview_apply_patch_renders_unified_diff() {
    let call = ToolCall {
        call_id: "c".to_string(),
        name: "apply_patch".to_string(),
        arguments: json!({
            "patches": [{
                "path": "lib.rs",
                "search": "old contents",
                "replace": "new contents"
            }]
        }),
    };
    let lines = CatalogPreview.preview_lines(&req("apply_patch"), &call, &PathBuf::from("/tmp"));
    assert!(
        lines.iter().any(|line| matches!(
            line,
            PreviewLine::Diff { added: true, line } if line.contains("new contents")
        )),
        "preview missing + new contents: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| matches!(
            line,
            PreviewLine::Diff { added: false, line } if line.contains("old contents")
        )),
        "preview missing - old contents: {lines:?}"
    );
}

#[test]
fn preview_shell_highlights_command() {
    let call = ToolCall {
        call_id: "c".to_string(),
        name: "shell".to_string(),
        arguments: json!({ "command": "cargo test -p squeezy-agent" }),
    };
    let lines = CatalogPreview.preview_lines(&req("shell"), &call, &PathBuf::from("/tmp"));
    assert!(
        matches!(
            lines.first(),
            Some(PreviewLine::Highlighted {
                lang: "shell",
                text,
            }) if text == "cargo test -p squeezy-agent"
        ),
        "expected shell-highlighted first line, got {lines:?}"
    );
}

#[test]
fn preview_webfetch_highlights_host() {
    let call = ToolCall {
        call_id: "c".to_string(),
        name: "webfetch".to_string(),
        arguments: json!({ "url": "https://api.github.com/repos/foo" }),
    };
    let lines = CatalogPreview.preview_lines(&req("webfetch"), &call, &PathBuf::from("/tmp"));
    assert!(
        matches!(
            lines.first(),
            Some(PreviewLine::Highlighted {
                lang: "url-host",
                text,
            }) if text == "api.github.com"
        ),
        "expected url-host highlight, got {lines:?}"
    );
}
