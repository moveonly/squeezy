use serde_json::Value;
use squeezy_core::SymbolKind;
use squeezy_graph::GraphSymbol;

use crate::graph_tools_filters::path_matches_filter;

/// True when a path looks like test code. Recognises the common per-language
/// conventions: a `test`/`tests`/`__tests__`/`spec`/`testing` directory
/// segment, or a file name ending in a test/spec suffix
/// (`_test`/`_tests`/`.test`/`.spec`/`Test`/`Tests`/`Spec`). Used by the
/// `exclude_tests`/`tests_only` scoping shared across the read tools.
pub(crate) fn path_is_test(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let dir_marker = lower.split('/').any(|segment| {
        matches!(
            segment,
            "test" | "tests" | "__tests__" | "spec" | "specs" | "testing"
        )
    });
    if dir_marker {
        return true;
    }
    let file = lower.rsplit('/').next().unwrap_or(lower.as_str());
    // Strip a trailing extension so `foo_test.rs` / `foo.test.ts` both match.
    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
    stem.ends_with("_test")
        || stem.ends_with("_tests")
        || stem.ends_with(".test")
        || stem.ends_with(".spec")
        || stem.ends_with("test")
        || stem.ends_with("tests")
        || stem.ends_with("spec")
}

/// True when a symbol is part of test code: either its kind is `Test` or its
/// declaring file path looks like test code (see [`path_is_test`]).
pub(crate) fn symbol_is_test(symbol: &GraphSymbol) -> bool {
    symbol.kind == SymbolKind::Test || path_is_test(symbol.file_id.0.as_str())
}

/// Apply an `exclude_tests`/`tests_only` pair to a test-ness verdict. When both
/// are set, `tests_only` wins (the more specific request). Returns whether the
/// item should be KEPT.
pub(crate) fn passes_test_scope(is_test: bool, exclude_tests: bool, tests_only: bool) -> bool {
    if tests_only {
        is_test
    } else if exclude_tests {
        !is_test
    } else {
        true
    }
}

/// Best-effort extraction of the workspace-relative path a result packet points
/// at, for the `result_path` filter on the flow/hierarchy/symbol_context tools.
/// Looks at the packet bodies these tools emit: `symbol.path`, `reference.path`,
/// the first `spans[].path`, and (for edge packets) the `edge.from` symbol's
/// file via the `spans` entry. Returns `None` when no path can be determined.
pub(crate) fn packet_path(packet: &Value) -> Option<&str> {
    if let Some(path) = packet
        .get("symbol")
        .and_then(|symbol| symbol.get("path"))
        .and_then(Value::as_str)
    {
        return Some(path);
    }
    if let Some(path) = packet
        .get("reference")
        .and_then(|reference| reference.get("path"))
        .and_then(Value::as_str)
    {
        return Some(path);
    }
    if let Some(path) = packet
        .get("caller")
        .and_then(|caller| caller.get("path"))
        .and_then(Value::as_str)
    {
        return Some(path);
    }
    packet
        .get("spans")
        .and_then(Value::as_array)
        .and_then(|spans| spans.first())
        .and_then(|span| span.get("path"))
        .and_then(Value::as_str)
}

/// True when a result packet passes the optional `result_path` scope. Packets
/// whose path can't be determined are KEPT (the filter is a positive scope, not
/// a hard gate that would silently drop edges the extractor couldn't anchor).
pub(crate) fn packet_matches_result_path(packet: &Value, filter: Option<&str>) -> bool {
    let Some(filter) = filter.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    match packet_path(packet) {
        Some(path) => path_matches_filter(path, filter),
        None => true,
    }
}

/// Pick the effective path scope for RESULT packets on the flow/hierarchy tools.
/// An explicit `result_path` wins (it lets a caller decouple the root scope from
/// the result scope); otherwise the plain `path` argument scopes the results too,
/// satisfying the "path scopes RESULT packets" contract. Empty/whitespace tokens
/// are treated as absent so a blank string never collapses the result set.
pub(crate) fn result_path_scope<'a>(
    path: Option<&'a str>,
    result_path: Option<&'a str>,
) -> Option<&'a str> {
    result_path
        .or(path)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// True when a result packet passes the `exclude_tests`/`tests_only` scope,
/// keyed on the packet's path (see [`packet_path`]/[`path_is_test`]). Packets
/// with no determinable path are KEPT under `exclude_tests` and DROPPED under
/// `tests_only` (a path-less packet can't be confirmed as a test).
pub(crate) fn packet_matches_test_scope(
    packet: &Value,
    exclude_tests: bool,
    tests_only: bool,
) -> bool {
    if !exclude_tests && !tests_only {
        return true;
    }
    match packet_path(packet) {
        Some(path) => passes_test_scope(path_is_test(path), exclude_tests, tests_only),
        None => !tests_only,
    }
}
