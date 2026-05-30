//! Per-tool [`PermissionPreview`] catalog. Turns a `ToolCall` into a
//! tool-specific `Vec<PreviewLine>` that the TUI renders with per-kind
//! styling — one approval dialog per tool family, all routed through a
//! single TUI renderer instead of one widget per tool.
//!
//! The trait exists so MCP transports / future tool families can plug in
//! their own preview without touching this file; the in-tree catalog
//! covers the first-party tools and is dispatched from
//! [`crate::ToolRegistry::preview_for`].
use std::path::Path;

use serde_json::Value;
use squeezy_core::PermissionRequest;

use crate::{
    ToolCall,
    patch::{ApplyPatchArgs, ApplyPatchOperation},
    shell::ShellArgs,
    truncate_text,
    web::{WebFetchArgs, WebSearchArgs, web_url_host},
};

/// One row of the rendered preview. Variants are styling hints, not
/// markup — the TUI translates them to ANSI colors (Diff → red/green,
/// Highlighted → tree-sitter palette, Warning → orange, Plain → fg
/// default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewLine {
    Plain { text: String },
    Diff { added: bool, line: String },
    Highlighted { lang: &'static str, text: String },
    Warning { text: String },
}

/// Per-tool preview hook. Implementations are pure functions of the
/// permission request, the underlying tool call, and the workspace
/// root — they MUST NOT perform I/O.
pub trait PermissionPreview {
    fn preview_lines(
        &self,
        request: &PermissionRequest,
        call: &ToolCall,
        workspace_root: &Path,
    ) -> Vec<PreviewLine>;
}

const LINE_BUDGET: usize = 240;
const MAX_HUNK_LINES: usize = 20;

/// In-tree catalog. Dispatches on `call.name` and falls back to a
/// metadata dump for tools without a bespoke preview.
pub struct CatalogPreview;

impl PermissionPreview for CatalogPreview {
    fn preview_lines(
        &self,
        request: &PermissionRequest,
        call: &ToolCall,
        _workspace_root: &Path,
    ) -> Vec<PreviewLine> {
        match call.name.as_str() {
            "apply_patch" => apply_patch_preview(call),
            "write_file" => write_file_preview(call),
            "shell" => shell_preview(call),
            "webfetch" => webfetch_preview(call),
            "websearch" => websearch_preview(call),
            _ => fallback_preview(request),
        }
    }
}

fn push_diff(out: &mut Vec<PreviewLine>, body: &str, added: bool) {
    let prefix = if added { '+' } else { '-' };
    for line in body.lines().take(MAX_HUNK_LINES) {
        out.push(PreviewLine::Diff {
            added,
            line: format!("{prefix} {}", truncate_text(line, LINE_BUDGET)),
        });
    }
}

fn apply_patch_preview(call: &ToolCall) -> Vec<PreviewLine> {
    let Ok(args) = serde_json::from_value::<ApplyPatchArgs>(call.arguments.clone()) else {
        return vec![PreviewLine::Warning {
            text: "apply_patch arguments did not parse — preview unavailable".to_string(),
        }];
    };
    let mut out = Vec::new();
    for patch in &args.patches {
        out.push(PreviewLine::Highlighted {
            lang: "path",
            text: patch.path.clone(),
        });
        push_diff(&mut out, &patch.search, false);
        push_diff(&mut out, &patch.replace, true);
    }
    for op in &args.operations {
        match op {
            ApplyPatchOperation::SearchReplace {
                path,
                search,
                replace,
                ..
            } => {
                out.push(PreviewLine::Highlighted {
                    lang: "path",
                    text: path.clone(),
                });
                push_diff(&mut out, search, false);
                push_diff(&mut out, replace, true);
            }
            ApplyPatchOperation::CreateFile { path, contents, .. } => {
                out.push(PreviewLine::Highlighted {
                    lang: "path",
                    text: format!("create {path}"),
                });
                push_diff(&mut out, contents, true);
            }
            ApplyPatchOperation::DeleteFile { path, .. } => out.push(PreviewLine::Warning {
                text: format!("delete {path}"),
            }),
            ApplyPatchOperation::MoveFile { from, to, .. } => out.push(PreviewLine::Highlighted {
                lang: "path",
                text: format!("move {from} -> {to}"),
            }),
        }
    }
    if out.is_empty() {
        out.push(PreviewLine::Warning {
            text: "apply_patch has no operations — preview empty".to_string(),
        });
    }
    out
}

fn write_file_preview(call: &ToolCall) -> Vec<PreviewLine> {
    let path = call
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("*");
    let content = call
        .arguments
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut out = vec![PreviewLine::Highlighted {
        lang: "path",
        text: path.to_string(),
    }];
    push_diff(&mut out, content, true);
    out
}

fn shell_preview(call: &ToolCall) -> Vec<PreviewLine> {
    let Ok(args) = serde_json::from_value::<ShellArgs>(call.arguments.clone()) else {
        return vec![PreviewLine::Warning {
            text: "shell arguments did not parse — preview unavailable".to_string(),
        }];
    };
    let mut out = vec![PreviewLine::Highlighted {
        lang: "shell",
        text: args.command.clone(),
    }];
    if let Some(workdir) = args.workdir.as_deref() {
        out.push(PreviewLine::Plain {
            text: format!("cwd: {workdir}"),
        });
    }
    let analysis = crate::shell_parse::analyze_shell_command(&args.command);
    if analysis.network {
        out.push(PreviewLine::Warning {
            text: "command appears to access the network".to_string(),
        });
    }
    if analysis.destructive {
        out.push(PreviewLine::Warning {
            text: "command flagged as destructive by static analysis".to_string(),
        });
    }
    out
}

fn webfetch_preview(call: &ToolCall) -> Vec<PreviewLine> {
    let Ok(args) = serde_json::from_value::<WebFetchArgs>(call.arguments.clone()) else {
        return vec![PreviewLine::Warning {
            text: "webfetch arguments did not parse — preview unavailable".to_string(),
        }];
    };
    let host = web_url_host(&args.url).unwrap_or_else(|_| "?".to_string());
    vec![
        PreviewLine::Highlighted {
            lang: "url-host",
            text: host,
        },
        PreviewLine::Plain {
            text: truncate_text(&args.url, LINE_BUDGET),
        },
    ]
}

fn websearch_preview(call: &ToolCall) -> Vec<PreviewLine> {
    let Ok(args) = serde_json::from_value::<WebSearchArgs>(call.arguments.clone()) else {
        return vec![PreviewLine::Warning {
            text: "websearch arguments did not parse — preview unavailable".to_string(),
        }];
    };
    vec![PreviewLine::Highlighted {
        lang: "query",
        text: truncate_text(&args.query, LINE_BUDGET),
    }]
}

fn fallback_preview(request: &PermissionRequest) -> Vec<PreviewLine> {
    let mut out = vec![PreviewLine::Plain {
        text: request.summary.clone(),
    }];
    for (key, value) in &request.metadata {
        out.push(PreviewLine::Plain {
            text: format!("{key}: {}", truncate_text(value, LINE_BUDGET)),
        });
    }
    out
}

#[cfg(test)]
#[path = "preview_tests.rs"]
mod tests;
