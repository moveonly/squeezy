use serde_json::{Value, json};

#[derive(Debug, Clone, Copy)]
pub(crate) enum DiffNextActionKind {
    ReadSlice,
    SymbolContextOrSlice { rust_graph: bool },
}

pub(crate) fn read_diff_next_action(path: &str, kind: DiffNextActionKind) -> Value {
    match kind {
        DiffNextActionKind::ReadSlice => json!({
            "tool": "read_slice",
            "arguments": {
                "path": path,
                "read_mode": "slice"
            },
            "reason": "read the exact current source slice if surrounding context is needed"
        }),
        DiffNextActionKind::SymbolContextOrSlice { rust_graph: true } => json!({
            "tool": "symbol_context",
            "arguments": {
                "path": path
            },
            "reason": "look up the enclosing symbol's callers and callees instead of refetching the same diff bytes"
        }),
        DiffNextActionKind::SymbolContextOrSlice { rust_graph: false } => json!({
            "tool": "read_slice",
            "arguments": {
                "path": path,
                "read_mode": "slice"
            },
            "reason": "read additional surrounding source if context beyond the diff bytes is needed"
        }),
    }
}
