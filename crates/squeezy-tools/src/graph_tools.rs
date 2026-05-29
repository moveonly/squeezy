use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs,
    path::Path,
};

use serde::Deserialize;
use serde_json::{Value, json};
use squeezy_core::{
    Confidence, EdgeKind, FileId, Freshness, LanguageKind, Provenance, SourceSpan, SymbolId,
    SymbolKind,
};
use squeezy_graph::{
    CallEdgeHit, CargoDiagnosticHit, CargoFactFreshness, CargoFactsSummary, DirtyAnnotation,
    DirtyRange, GraphEdge, GraphManager, GraphSymbol, HierarchyNode, ReferenceHit, SignatureQuery,
};
use squeezy_vcs::{
    DiffFileStatus, DiffHunk, DiffMode, DiffOptions, DiffSnapshot, canonicalize_workspace_root,
};
use squeezy_workspace::{ExclusionReason, IndexCoverage};

use crate::{
    DEFAULT_GRAPH_MAX_DEPTH, DEFAULT_GRAPH_MAX_RESULTS, DEFAULT_READ_LIMIT,
    GRAPH_READ_SLICE_MAX_LINE_SCAN_BYTES, GRAPH_READY_WAIT, MAX_GRAPH_MAX_DEPTH,
    MAX_GRAPH_MAX_RESULTS, MAX_READ_LIMIT, POLICY_PREFIX_BYTES, ToolCall, ToolCostHint,
    ToolRegistry, ToolResult, ToolStatus, diff_mode_str, diff_path_set, diff_status_str, file_len,
    is_secret_path, make_result, read_prefix, read_range, sha256_file, tool_arg_error, tool_error,
    workspace_path,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SymbolContextArgs {
    pub(crate) query: String,
    path: Option<String>,
    diff_only: Option<bool>,
    mode: Option<DiffMode>,
    max_references: Option<usize>,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoMapArgs {
    max_depth: Option<usize>,
    max_files: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclSearchArgs {
    query: Option<String>,
    kind: Option<String>,
    path: Option<String>,
    language: Option<String>,
    visibility: Option<String>,
    attribute: Option<String>,
    max_results: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DefinitionSearchArgs {
    query: Option<String>,
    symbol_id: Option<String>,
    kind: Option<String>,
    path: Option<String>,
    language: Option<String>,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceSearchArgs {
    query: Option<String>,
    text: Option<String>,
    symbol_id: Option<String>,
    path: Option<String>,
    max_results: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlowArgs {
    symbol_id: Option<String>,
    query: Option<String>,
    kind: Option<String>,
    path: Option<String>,
    target_symbol_id: Option<String>,
    target_query: Option<String>,
    max_depth: Option<usize>,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HierarchyArgs {
    symbol_id: Option<String>,
    query: Option<String>,
    kind: Option<String>,
    path: Option<String>,
    max_depth: Option<usize>,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadSliceArgs {
    pub(crate) path: Option<String>,
    symbol_id: Option<String>,
    span_kind: Option<ReadSliceSpanKind>,
    read_mode: Option<ReadSliceReadMode>,
    diff_baseline: Option<DiffReadBaseline>,
    max_ranges: Option<usize>,
    start_byte: Option<usize>,
    end_byte: Option<usize>,
    start_line: Option<u32>,
    end_line: Option<u32>,
    context_lines: Option<u32>,
    offset: Option<usize>,
    limit: Option<usize>,
    diff_only: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReadSliceReadMode {
    #[default]
    Slice,
    Diff,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DiffReadBaseline {
    #[default]
    Worktree,
    #[serde(alias = "branch")]
    BranchBase,
    Index,
    LastReceipt,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReadSliceSpanKind {
    #[default]
    Signature,
    Body,
}
fn coverage_json(coverage: &IndexCoverage) -> Option<Value> {
    if coverage.skipped_files == 0 && coverage.skipped_dirs == 0 && coverage.reasons.is_empty() {
        return None;
    }
    let reasons = coverage
        .reasons
        .iter()
        .map(|(reason, coverage)| {
            (
                reason.clone(),
                json!({
                    "files": coverage.files,
                    "dirs": coverage.dirs,
                    "bytes": coverage.bytes,
                    "samples": coverage.samples,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    Some(json!({
        "skipped_files": coverage.skipped_files,
        "skipped_dirs": coverage.skipped_dirs,
        "skipped_bytes": coverage.skipped_bytes,
        "reasons": reasons,
    }))
}
fn diff_read_baseline_str(baseline: DiffReadBaseline) -> &'static str {
    match baseline {
        DiffReadBaseline::Worktree => "worktree",
        DiffReadBaseline::BranchBase => "branch_base",
        DiffReadBaseline::Index => "index",
        DiffReadBaseline::LastReceipt => "last_receipt",
    }
}

enum LastReceiptDiffOutcome {
    Result(Box<ToolResult>),
    Fallback(&'static str),
}

/// Bundle of arguments shared by the three `read_mode=diff` helpers. Grouping
/// them keeps each helper under `clippy::too_many_arguments` and removes the
/// duplicated argument forwarding between `execute_read_slice_diff_blocking`
/// → `read_slice_git_diff` / `read_slice_last_receipt_diff` plus the
/// `LastReceipt` → `Worktree` fallback re-call.
struct ReadSliceDiffCtx<'a> {
    call: &'a ToolCall,
    args: &'a ReadSliceArgs,
    path: &'a Path,
    rel: &'a str,
    graph_available: bool,
    graph_status: &'static str,
    confidence: Confidence,
    provenance: Vec<Provenance>,
    span: Option<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChangedByteRange {
    start: usize,
    end: usize,
    start_line: u32,
    end_line: u32,
    status: &'static str,
}

impl ChangedByteRange {
    fn new(start: usize, end: usize, start_line: u32, end_line: u32, status: &'static str) -> Self {
        Self {
            start,
            end,
            start_line,
            end_line,
            status,
        }
    }
}

/// Next-action recommendation for a diff packet. Diff ranges that already
/// carry the changed bytes inline should not steer the model back to
/// `read_slice` in slice mode for the same path — that just re-fetches the
/// same content. Prefer `symbol_context` (Rust graph available) so the model
/// can pull the enclosing symbol's callers/callees instead.
#[derive(Debug, Clone, Copy)]
enum DiffNextActionKind {
    /// Recommend the slice mode of `read_slice` for cases that did not
    /// already include source bytes (binary files, deleted files, empty
    /// range lists). Surrounding context still needs a real fetch.
    ReadSlice,
    /// Recommend either `symbol_context` (when the Rust semantic graph is
    /// available for this path) or the slice mode of `read_slice` (as a
    /// language-agnostic fallback). Used when the diff range already includes
    /// `content` inline.
    SymbolContextOrSlice { rust_graph: bool },
}

fn read_diff_next_action(path: &str, kind: DiffNextActionKind) -> Value {
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

fn read_diff_packet(
    path: &str,
    span: Option<SourceSpan>,
    claim: &'static str,
    confidence: Confidence,
    provenance: &[Provenance],
    cost_hint: ToolCostHint,
    next_action_kind: DiffNextActionKind,
) -> Value {
    evidence_packet(
        claim,
        vec![span_for_path_json(path, span)],
        confidence,
        Freshness::Fresh,
        provenance.to_vec(),
        cost_hint,
        read_diff_next_action(path, next_action_kind),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChangedLineRange {
    start_line: u32,
    end_line: u32,
    status: &'static str,
}

fn changed_byte_ranges_from_patch(patch: &str, text: &str) -> Vec<ChangedByteRange> {
    let mut line_ranges = Vec::<ChangedLineRange>::new();
    let mut new_line = 0u32;
    for line in patch.lines() {
        if line.starts_with("@@") {
            new_line = parse_hunk_new_start(line).unwrap_or(1);
            continue;
        }
        if new_line == 0 || line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            push_changed_line(&mut line_ranges, new_line, "modified");
            new_line = new_line.saturating_add(1);
        } else if line.starts_with('-') {
            push_changed_line(&mut line_ranges, new_line.max(1), "deleted");
        } else if line.starts_with(' ') {
            new_line = new_line.saturating_add(1);
        }
    }
    let modified_ranges = line_ranges
        .iter()
        .filter(|range| range.status == "modified")
        .cloned()
        .collect::<Vec<_>>();
    line_ranges
        .into_iter()
        .filter(|range| {
            range.status != "deleted"
                || !modified_ranges.iter().any(|modified| {
                    range.start_line <= modified.end_line && range.end_line >= modified.start_line
                })
        })
        .map(|range| line_range_to_byte_range(text, range))
        .collect()
}

fn push_changed_line(ranges: &mut Vec<ChangedLineRange>, line: u32, status: &'static str) {
    if let Some(last) = ranges.last_mut()
        && last.status == status
        && line <= last.end_line.saturating_add(1)
    {
        last.end_line = last.end_line.max(line);
        return;
    }
    ranges.push(ChangedLineRange {
        start_line: line,
        end_line: line,
        status,
    });
}

fn parse_hunk_new_start(line: &str) -> Option<u32> {
    let plus = line.find('+')?;
    let rest = line.get(plus + 1..)?;
    let end = rest
        .find(|ch: char| ch == ',' || ch.is_ascii_whitespace())
        .unwrap_or(rest.len());
    rest.get(..end)?.parse().ok()
}

fn diff_hunks_to_byte_ranges(hunks: &[DiffHunk], text: &str) -> Vec<ChangedByteRange> {
    hunks
        .iter()
        .map(|hunk| {
            let start_line = hunk.start_line.saturating_add(1).max(1);
            let end_line = hunk.end_line.saturating_add(1).max(start_line);
            line_range_to_byte_range(
                text,
                ChangedLineRange {
                    start_line,
                    end_line,
                    status: "modified",
                },
            )
        })
        .collect()
}

fn line_range_to_byte_range(text: &str, range: ChangedLineRange) -> ChangedByteRange {
    let offsets = line_start_offsets(text);
    let start = byte_for_line(&offsets, text.len(), range.start_line);
    let end = if range.status == "deleted" {
        start
    } else {
        byte_after_line(&offsets, text.len(), range.end_line)
    };
    ChangedByteRange::new(
        start,
        end.max(start),
        range.start_line,
        range.end_line,
        range.status,
    )
}

fn line_start_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0usize];
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' && index + 1 < text.len() {
            offsets.push(index + 1);
        }
    }
    offsets
}

fn byte_for_line(offsets: &[usize], text_len: usize, line: u32) -> usize {
    offsets
        .get(line.saturating_sub(1) as usize)
        .copied()
        .unwrap_or(text_len)
}

fn byte_after_line(offsets: &[usize], text_len: usize, line: u32) -> usize {
    offsets.get(line as usize).copied().unwrap_or(text_len)
}

fn byte_diff_ranges(old: &[u8], new: &[u8]) -> Vec<ChangedByteRange> {
    if old == new {
        return Vec::new();
    }
    let mut prefix = 0usize;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut old_suffix = old.len();
    let mut new_suffix = new.len();
    while old_suffix > prefix && new_suffix > prefix && old[old_suffix - 1] == new[new_suffix - 1] {
        old_suffix -= 1;
        new_suffix -= 1;
    }
    let mut start = prefix;
    while start > 0 && new[start - 1] != b'\n' {
        start -= 1;
    }
    let mut end = new_suffix.max(start);
    while end < new.len() && new[end.saturating_sub(1)] != b'\n' {
        end += 1;
    }
    // Compute line numbers honestly so the resulting range stands on its own
    // (callers can still overwrite, but the type no longer lies about being
    // line-aware). `new` here is the window-local current bytes; the caller
    // is responsible for offsetting to file-absolute lines if needed.
    let start_line = line_number_for_byte_bytes(new, start);
    let end_line =
        line_number_for_byte_bytes(new, end.saturating_sub(1).max(start)).max(start_line);
    vec![ChangedByteRange::new(
        start, end, start_line, end_line, "modified",
    )]
}

fn line_number_for_byte(text: &str, byte: usize) -> u32 {
    line_number_for_byte_bytes(text.as_bytes(), byte)
}

fn line_number_for_byte_bytes(bytes: &[u8], byte: usize) -> u32 {
    let clamped = byte.min(bytes.len());
    bytes[..clamped]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        .saturating_add(1) as u32
}

/// Count the number of newlines strictly before `offset` in `path`. Used to
/// promote window-local line numbers to file-absolute ones in
/// `read_slice_last_receipt_diff` without slurping the full file into memory
/// twice. Returns the count of `\n` bytes in `[0, offset)`.
fn window_line_offset(path: &Path, offset: usize) -> std::result::Result<u32, std::io::Error> {
    if offset == 0 {
        return Ok(0);
    }
    use std::io::{BufReader, Read};
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut remaining = offset;
    let mut buf = [0u8; 8192];
    let mut newlines: u32 = 0;
    while remaining > 0 {
        let to_read = remaining.min(buf.len());
        let read = reader.read(&mut buf[..to_read])?;
        if read == 0 {
            break;
        }
        newlines = newlines
            .saturating_add(buf[..read].iter().filter(|byte| **byte == b'\n').count() as u32);
        remaining -= read;
    }
    Ok(newlines)
}
fn symbol_matches_path_filter(symbol: &GraphSymbol, filter: Option<&str>) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    let path = symbol.file_id.0.as_str();
    if path == filter || path.ends_with(&format!("/{filter}")) {
        return true;
    }
    // Append fuzzy path matching as a fallback so casual queries like
    // `path: "graph_mgr"` resolve to `crates/squeezy-graph/src/lib.rs`.
    // Suffix matching above keeps precedence; fuzzy only rescues misses.
    squeezy_rank::fuzzy::fuzzy_path_score(path, filter).is_some()
}

fn annotate_graph(manager: &mut GraphManager, snapshot: &DiffSnapshot) {
    let dirty = snapshot
        .files
        .iter()
        .map(|file| {
            let ranges = if file.hunks.is_empty() {
                vec![DirtyRange {
                    start_line: 0,
                    end_line: u32::MAX,
                }]
            } else {
                file.hunks
                    .iter()
                    .map(|hunk| DirtyRange {
                        start_line: hunk.start_line,
                        end_line: hunk.end_line,
                    })
                    .collect()
            };
            (
                FileId::new(file.path.clone()),
                DirtyAnnotation {
                    status: diff_status_str(file.status).to_string(),
                    ranges,
                },
            )
        })
        .collect::<HashMap<_, _>>();
    manager.graph_mut().annotate_dirty_ranges(&dirty);
}

fn symbol_context_json(
    graph: &squeezy_graph::SemanticGraph,
    symbol: &GraphSymbol,
    max_references: usize,
) -> Value {
    let references = graph
        .references_to_symbol(&symbol.id)
        .into_iter()
        .take(max_references)
        .map(reference_json)
        .collect::<Vec<_>>();
    let callers = graph
        .callers(&symbol.id)
        .into_iter()
        .take(max_references)
        .filter_map(|hit| hit.caller)
        .map(|caller| {
            json!({
                "id": caller.id.0,
                "name": caller.name,
                "kind": format!("{:?}", caller.kind),
                "path": caller.file_id.0,
                "span": span_json(caller.span),
            })
        })
        .collect::<Vec<_>>();
    let diagnostics = graph
        .cargo_diagnostics_for_symbol(symbol)
        .into_iter()
        .take(max_references)
        .map(|hit| cargo_diagnostic_hit_json(&hit))
        .collect::<Vec<_>>();
    json!({
        "id": symbol.id.0,
        "name": symbol.name,
        "kind": format!("{:?}", symbol.kind),
        "path": symbol.file_id.0,
        "signature": symbol.signature,
        "visibility": symbol.visibility,
        "span": span_json(symbol.span),
        "dirty": symbol.dirty.as_ref().map(|dirty| json!({
            "status": dirty.status,
            "ranges": dirty.ranges.iter().map(|range| json!({
                "start_line": range.start_line,
                "end_line": range.end_line,
            })).collect::<Vec<_>>(),
        })),
        "references": references,
        "callers": callers,
        "diagnostics": diagnostics,
        "confidence": symbol.confidence.id(),
        "freshness": format!("{:?}", symbol.freshness),
    })
}

fn graph_tool_diff_mode(call: &ToolCall) -> DiffMode {
    if call.name == "symbol_context" {
        serde_json::from_value::<SymbolContextArgs>(call.arguments.clone())
            .ok()
            .and_then(|args| args.mode)
            .unwrap_or_default()
    } else {
        DiffMode::Worktree
    }
}

pub(crate) fn graph_unavailable_result(call: &ToolCall) -> ToolResult {
    make_result(
        call,
        ToolStatus::Success,
        json!({
            "tool": call.name,
            "graph_available": false,
            "reason": "semantic graph is unavailable for this workspace",
            "packets": [],
            "fallback": {
                "status": "graph_unavailable",
                "suggested_tools": [
                    {"tool": "glob", "arguments": {"pattern": "**/*"}},
                    {"tool": "grep", "arguments": {"pattern": "<query>", "output_mode": "files_with_matches"}}
                ]
            }
        }),
        ToolCostHint::default(),
        None,
    )
}

pub(crate) fn graph_payload(
    tool: &str,
    manager: &GraphManager,
    refresh: &squeezy_graph::RefreshReport,
) -> serde_json::Map<String, Value> {
    let mut payload = serde_json::Map::new();
    payload.insert("tool".to_string(), json!(tool));
    payload.insert("graph_available".to_string(), json!(true));
    payload.insert("refresh".to_string(), refresh_report_json(refresh));
    if let Some(coverage) = coverage_json(&manager.build_report().coverage) {
        payload.insert("coverage".to_string(), coverage);
    }
    payload
}

fn refresh_report_json(report: &squeezy_graph::RefreshReport) -> Value {
    // Intentionally omits `duration_ms`: that field changes between otherwise
    // identical calls and breaks the receipt-stub layer for graph tools.
    // Telemetry still records wall-clock timing via the typed graph event.
    json!({
        "refreshed": report.refreshed,
        "changed_files": report.changed_files.iter().map(|id| id.0.clone()).collect::<Vec<_>>(),
        "removed_files": report.removed_files.iter().map(|id| id.0.clone()).collect::<Vec<_>>(),
        "reparsed_files": report.reparsed_files,
        "excluded_files": report.excluded_files,
        "excluded_dirs": report.excluded_dirs,
        "excluded_bytes": report.excluded_bytes,
        "bytes_reparsed": report.bytes_reparsed,
        "skipped_due_to_interval": report.skipped_due_to_interval,
        "budget_exhausted": report.budget_exhausted,
    })
}

fn graph_stats_json(graph: &squeezy_graph::SemanticGraph) -> Value {
    let stats = graph.stats();
    json!({
        "files": stats.files,
        "symbols": stats.symbols,
        "edges": stats.edges,
        "body_hits": stats.body_hits,
        "references": stats.references,
        "calls": stats.calls,
        "cargo_workspaces": stats.cargo_workspaces,
        "cargo_packages": stats.cargo_packages,
        "cargo_targets": stats.cargo_targets,
        "cargo_features": stats.cargo_features,
        "cargo_diagnostics": stats.cargo_diagnostics,
        "body_hit_trigram_indexed": stats.body_hit_trigram_indexed,
        "body_hit_trigram_terms": stats.body_hit_trigram_terms,
        "reference_index_terms": stats.reference_index_terms,
    })
}

pub(crate) fn cargo_facts_summary_json(summary: &CargoFactsSummary) -> Value {
    json!({
        "workspaces": summary.workspaces,
        "packages": summary.packages,
        "targets": summary.targets,
        "features": summary.features,
        "diagnostics": summary.diagnostics,
        "freshness": summary.freshness.as_ref().map(cargo_freshness_json),
    })
}

fn cargo_freshness_json(freshness: &CargoFactFreshness) -> Value {
    json!({
        "status": format!("{:?}", freshness.status),
        "input_fingerprint": freshness.input_fingerprint.0,
        "current_fingerprint": freshness.current_fingerprint.0,
        "stale_reasons": freshness.stale_reasons,
    })
}

fn cargo_diagnostic_hit_json(hit: &CargoDiagnosticHit) -> Value {
    let diagnostic = &hit.diagnostic;
    json!({
        "level": diagnostic.level,
        "message": diagnostic.message,
        "code": diagnostic.code,
        "path": diagnostic.file_id.as_ref().map(|id| id.0.clone()),
        "span": diagnostic.span.map(span_json),
        "label": diagnostic.label,
        "package_id": diagnostic.package_id,
        "target_name": diagnostic.target_name,
        "freshness": cargo_freshness_json(&hit.freshness),
        "provenance": provenance_json(diagnostic.provenance.clone()),
    })
}

fn graph_language_counts_json(graph: &squeezy_graph::SemanticGraph) -> Value {
    let mut counts = BTreeMap::<String, usize>::new();
    for file in graph.files.values() {
        *counts
            .entry(file.language.display_name().to_string())
            .or_default() += 1;
    }
    json!(counts)
}

fn graph_limit(limit: Option<usize>) -> usize {
    limit
        .unwrap_or(DEFAULT_GRAPH_MAX_RESULTS)
        .clamp(1, MAX_GRAPH_MAX_RESULTS)
}

fn decl_search_has_query_or_filter(args: &DeclSearchArgs) -> bool {
    [
        args.query.as_deref(),
        args.kind.as_deref(),
        args.path.as_deref(),
        args.language.as_deref(),
        args.visibility.as_deref(),
        args.attribute.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| !value.trim().is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SymbolKindFilter {
    Single(SymbolKind),
    Callable,
}

fn parse_symbol_kind_filter(value: &str) -> Option<SymbolKindFilter> {
    let normalized = value.trim().to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "callable" | "callables" | "function_like" | "function-like" | "functions"
    ) {
        return Some(SymbolKindFilter::Callable);
    }
    parse_symbol_kind(value).map(SymbolKindFilter::Single)
}

fn single_symbol_kind(filter: Option<SymbolKindFilter>) -> Option<SymbolKind> {
    match filter {
        Some(SymbolKindFilter::Single(kind)) => Some(kind),
        _ => None,
    }
}

fn symbol_matches_kind_filter(kind: SymbolKind, filter: Option<SymbolKindFilter>) -> bool {
    match filter {
        None => true,
        Some(SymbolKindFilter::Single(expected)) => kind == expected,
        Some(SymbolKindFilter::Callable) => matches!(
            kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Test
        ),
    }
}

fn symbol_matches_visibility_filter(symbol: &GraphSymbol, visibility: Option<&str>) -> bool {
    let Some(visibility) = visibility.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    symbol
        .visibility
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case(visibility))
}

fn symbol_matches_attribute_filter(symbol: &GraphSymbol, attribute: Option<&str>) -> bool {
    let Some(attribute) = attribute.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    symbol
        .attributes
        .iter()
        .any(|value| value.eq_ignore_ascii_case(attribute) || value.contains(attribute))
}

fn graph_symbol_search(
    graph: &squeezy_graph::SemanticGraph,
    query: Option<&str>,
    kind: Option<&str>,
    path: Option<&str>,
    language: Option<&str>,
    visibility: Option<&str>,
    attribute: Option<&str>,
) -> Vec<GraphSymbol> {
    let query = query.map(str::trim).filter(|value| !value.is_empty());
    let kind_filter = kind.and_then(parse_symbol_kind_filter);
    let mut seen = HashSet::new();
    // Dotted query semantics: `Type.method` / `Module::function` /
    // `pkg.Type.method` should resolve via the parent_id graph edge,
    // not via subsequence matching that picks up unrelated symbols
    // whose names happen to share characters (e.g. `PQueue.add`
    // colliding with `QueueAddOptions`).
    let dotted_hits = query.and_then(|q| resolve_dotted_query(graph, q));
    let candidates = if let Some(matches) = dotted_hits {
        matches
    } else if let Some(query) = query {
        graph
            .signature_search(&SignatureQuery {
                text: query.to_string(),
                kind: single_symbol_kind(kind_filter),
                visibility: visibility.map(str::to_string),
                attribute: attribute.map(str::to_string),
            })
            .into_iter()
            .chain(graph.find_symbol_by_name(query))
            .collect::<Vec<_>>()
    } else {
        graph.symbols.values().cloned().collect::<Vec<_>>()
    };
    let mut symbols = candidates
        .into_iter()
        .filter(|symbol| seen.insert(symbol.id.clone()))
        .filter(|symbol| symbol_matches_kind_filter(symbol.kind, kind_filter))
        .filter(|symbol| symbol_matches_visibility_filter(symbol, visibility))
        .filter(|symbol| symbol_matches_attribute_filter(symbol, attribute))
        .filter(|symbol| symbol_matches_path_filter(symbol, path))
        .filter(|symbol| language_matches(graph, symbol, language))
        .collect::<Vec<_>>();

    // Fuzzy widening: when the trigram-anchored candidate pool is empty
    // but a query was provided, run a fuzzy subsequence scan over all
    // symbols so casual queries (`graphmgr → GraphManager`) still
    // resolve. This only runs on a miss so high-confidence behaviour is
    // unchanged.
    if symbols.is_empty()
        && let Some(query) = query
    {
        symbols = graph
            .symbols
            .values()
            .filter(|symbol| symbol_matches_kind_filter(symbol.kind, kind_filter))
            .filter(|symbol| symbol_matches_visibility_filter(symbol, visibility))
            .filter(|symbol| symbol_matches_attribute_filter(symbol, attribute))
            .filter(|symbol| symbol_matches_path_filter(symbol, path))
            .filter(|symbol| language_matches(graph, symbol, language))
            .filter(|symbol| {
                let view = squeezy_rank::GraphSymbolView {
                    name: symbol.name.as_str(),
                    signature: symbol.signature.as_str(),
                };
                squeezy_rank::symbol_rank::rank_symbol(view, query).0
                    != squeezy_rank::symbol_rank::RankTier::NoMatch
            })
            .cloned()
            .collect::<Vec<_>>();
    }

    symbols.sort_by(|left, right| {
        query
            .map(|query| symbol_rank(left, query).cmp(&symbol_rank(right, query)))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.file_id.0.cmp(&right.file_id.0))
            .then(left.span.start_byte.cmp(&right.span.start_byte))
    });
    symbols
}

/// Resolve a dotted-or-double-coloned query like `PQueue.add` or
/// `cobra::Command::Execute` into the concrete member symbol(s) by
/// walking `parent_id` edges. Returns `None` for plain-name queries
/// so the existing trigram/subsequence search runs unchanged.
///
/// This is the fix for triage-bug-12: without it,
/// `downstream_flow(query="PQueue.add")` was landing on
/// `QueueAddOptions` (a TypeAlias whose name happens to contain
/// "Add") instead of `PQueue::add` (the method on the class).
fn resolve_dotted_query(
    graph: &squeezy_graph::SemanticGraph,
    query: &str,
) -> Option<Vec<GraphSymbol>> {
    // Split on `.` or `::`. Require at least one separator and at
    // least two non-empty segments — otherwise this is a plain name.
    let separators: &[&str] = &["::", "."];
    let mut segments: Vec<&str> = vec![query];
    for sep in separators {
        segments = segments.into_iter().flat_map(|s| s.split(*sep)).collect();
    }
    let segments: Vec<&str> = segments.into_iter().filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return None;
    }
    // Walk parent chain: find every symbol matching the last segment;
    // for each, verify its parent chain (via parent_id) reaches a
    // symbol whose name matches segment[0]. Intermediate segments
    // (segments[1..n-1]) must also appear in the chain in order.
    let member = segments[segments.len() - 1];
    let mut member_candidates = graph.find_symbol_by_name(member);
    if member_candidates.is_empty() {
        return None;
    }
    let expected_chain: Vec<&str> = segments[..segments.len() - 1].to_vec();
    member_candidates.retain(|cand| {
        let mut want = expected_chain.iter().rev().peekable();
        let mut current = cand.parent_id.clone();
        while let Some(needed) = want.peek() {
            let Some(parent_id) = current.clone() else {
                return false;
            };
            let Some(parent) = graph.symbols.get(&parent_id) else {
                return false;
            };
            if parent.name.as_str() == **needed {
                want.next();
                current = parent.parent_id.clone();
            } else {
                current = parent.parent_id.clone();
            }
        }
        want.peek().is_none()
    });
    if member_candidates.is_empty() {
        None
    } else {
        Some(member_candidates)
    }
}

fn decl_counts_by_language(graph: &squeezy_graph::SemanticGraph, symbols: &[GraphSymbol]) -> Value {
    let mut counts = BTreeMap::<String, usize>::new();
    for symbol in symbols {
        let label = graph
            .files
            .get(&symbol.file_id)
            .map(|file| file.language.display_name())
            .unwrap_or("unknown");
        *counts.entry(label.to_string()).or_default() += 1;
    }
    json!(counts)
}

fn decl_counts_by_kind(symbols: &[GraphSymbol]) -> Value {
    let mut counts = BTreeMap::<String, usize>::new();
    for symbol in symbols {
        *counts
            .entry(symbol_kind_label(symbol.kind).to_string())
            .or_default() += 1;
    }
    json!(counts)
}

pub(crate) fn resolve_definition_candidates(
    graph: &squeezy_graph::SemanticGraph,
    symbol_id: Option<&str>,
    query: Option<&str>,
    kind: Option<&str>,
    path: Option<&str>,
    language: Option<&str>,
) -> Vec<GraphSymbol> {
    if let Some(symbol_id) = symbol_id {
        return graph
            .symbols
            .get(&SymbolId::new(symbol_id))
            .cloned()
            .into_iter()
            .collect();
    }
    let Some(query) = query else {
        return Vec::new();
    };
    graph_symbol_search(graph, Some(query), kind, path, language, None, None)
}

fn symbol_rank(symbol: &GraphSymbol, query: &str) -> usize {
    // Preserve the historical exact > case-insensitive > signature-substring
    // ordering. `squeezy_rank` adds two extra tiers (token-bag, fuzzy) that
    // recover near-miss queries like `graphmgr → GraphManager` without
    // changing the relative ordering of existing high-confidence hits.
    let view = squeezy_rank::GraphSymbolView {
        name: symbol.name.as_str(),
        signature: symbol.signature.as_str(),
    };
    squeezy_rank::symbol_rank::rank_symbol(view, query)
        .0
        .as_usize()
}

fn parse_symbol_kind(value: &str) -> Option<SymbolKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "class" => Some(SymbolKind::Class),
        "crate" => Some(SymbolKind::Crate),
        "file" => Some(SymbolKind::File),
        "interface" => Some(SymbolKind::Interface),
        "module" | "mod" => Some(SymbolKind::Module),
        "struct" => Some(SymbolKind::Struct),
        "enum" => Some(SymbolKind::Enum),
        "union" => Some(SymbolKind::Union),
        "trait" => Some(SymbolKind::Trait),
        "impl" => Some(SymbolKind::Impl),
        "function" | "fn" => Some(SymbolKind::Function),
        "method" => Some(SymbolKind::Method),
        "const" => Some(SymbolKind::Const),
        "static" => Some(SymbolKind::Static),
        "type_alias" | "typealias" | "type alias" => Some(SymbolKind::TypeAlias),
        "field" => Some(SymbolKind::Field),
        "variant" => Some(SymbolKind::Variant),
        "macro" => Some(SymbolKind::Macro),
        "test" => Some(SymbolKind::Test),
        "unknown" => Some(SymbolKind::Unknown),
        _ => None,
    }
}

fn symbol_kind_label(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Class => "class",
        SymbolKind::Crate => "crate",
        SymbolKind::File => "file",
        SymbolKind::Interface => "interface",
        SymbolKind::Module => "module",
        SymbolKind::Struct => "struct",
        SymbolKind::Enum => "enum",
        SymbolKind::Union => "union",
        SymbolKind::Trait => "trait",
        SymbolKind::Impl => "impl",
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Const => "const",
        SymbolKind::Static => "static",
        SymbolKind::TypeAlias => "type_alias",
        SymbolKind::Field => "field",
        SymbolKind::Variant => "variant",
        SymbolKind::Macro => "macro",
        SymbolKind::Test => "test",
        SymbolKind::Unknown => "unknown",
    }
}

fn language_matches(
    graph: &squeezy_graph::SemanticGraph,
    symbol: &GraphSymbol,
    language: Option<&str>,
) -> bool {
    let Some(language) = language else {
        return true;
    };
    let Some(file) = graph.files.get(&symbol.file_id) else {
        return false;
    };
    let language = language.trim().to_ascii_lowercase();
    file.language.display_name().to_ascii_lowercase() == language
        || format!("{:?}", file.language).to_ascii_lowercase() == language
        || file
            .language
            .family()
            .map(|family| family.id() == language)
            .unwrap_or(false)
}

fn unsupported_file_samples(graph: &squeezy_graph::SemanticGraph, limit: usize) -> Vec<Value> {
    graph
        .files
        .values()
        .filter(|file| matches!(file.language, LanguageKind::Unsupported | LanguageKind::Unknown))
        .take(limit)
        .map(|file| {
            json!({
                "path": file.relative_path,
                "language": file.language.display_name(),
                "status": graph_status_for_language(file.language),
                "suggested_tools": [
                    {"tool": "grep", "arguments": {"pattern": "<query>", "path": file.relative_path}},
                    {"tool": "read_file", "arguments": {"path": file.relative_path, "limit": DEFAULT_READ_LIMIT}}
                ]
            })
        })
        .collect()
}

/// Structured zero-result fallback for graph-anchored tools.
///
/// Returns `Value::Null` when the tool has at least one result packet
/// (callers always pass `packet_count`; the empty case is the only one
/// that needs a fallback). When `packet_count == 0`, emits an
/// `evidence-packet-shaped` object with:
/// - `status`: `"no_graph_evidence"`
/// - `reason`: one of `supported_language_no_match`, `path_unsupported`,
///   `path_unknown`, `no_path_scope`
/// - `path` / `language` (nullable)
/// - `suggested_tools`: a regex-escaped `grep` invocation plus a
///   `decl_search` retry shape
fn graph_zero_hit_fallback(
    graph: &squeezy_graph::SemanticGraph,
    path: Option<&str>,
    query: Option<&str>,
    packet_count: usize,
) -> Value {
    if packet_count > 0 {
        return Value::Null;
    }
    let (path_value, language_value, reason) = match path {
        Some(path) => {
            let file = graph.files.values().find(|file| {
                file.relative_path == path || file.relative_path.ends_with(&format!("/{path}"))
            });
            match file {
                Some(file) => {
                    let reason = match file.language {
                        LanguageKind::Unsupported => "path_unsupported",
                        LanguageKind::Unknown => "path_unknown",
                        _ => "supported_language_no_match",
                    };
                    (
                        Value::String(file.relative_path.clone()),
                        Value::String(file.language.display_name().to_string()),
                        reason,
                    )
                }
                None => (Value::String(path.to_string()), Value::Null, "path_unknown"),
            }
        }
        None => (Value::Null, Value::Null, "no_path_scope"),
    };
    let grep_path = match &path_value {
        Value::String(p) => p.clone(),
        _ => ".".to_string(),
    };
    let grep_pattern = match query {
        Some(q) if !q.is_empty() => regex::escape(q),
        _ => "<query>".to_string(),
    };
    let decl_query = query.unwrap_or("<query>").to_string();
    json!({
        "status": "no_graph_evidence",
        "reason": reason,
        "path": path_value,
        "language": language_value,
        "suggested_tools": [
            {"tool": "grep", "arguments": {"pattern": grep_pattern, "path": grep_path}},
            {"tool": "decl_search", "arguments": {"query": decl_query, "kind": null}}
        ]
    })
}

fn graph_status_for_language(language: LanguageKind) -> &'static str {
    match language {
        LanguageKind::Unsupported => "unsupported_language",
        LanguageKind::Unknown => "unknown_language",
        _ => "indexed",
    }
}

fn evidence_packet(
    claim: impl Into<String>,
    spans: Vec<Value>,
    confidence: Confidence,
    freshness: Freshness,
    provenance: Vec<Provenance>,
    cost_hint: ToolCostHint,
    next_action: Value,
) -> Value {
    json!({
        "claim": claim.into(),
        "spans": spans,
        "confidence": confidence.id(),
        "freshness": format!("{:?}", freshness),
        "provenance": provenance.into_iter().map(provenance_json).collect::<Vec<_>>(),
        "cost_hint": cost_hint,
        "next_action": next_action,
    })
}

fn provenance_json(provenance: Provenance) -> Value {
    json!({
        "source": provenance.source,
        "reason": provenance.reason,
    })
}

fn symbol_packet(
    graph: &squeezy_graph::SemanticGraph,
    symbol: &GraphSymbol,
    tool: &str,
    next_action: Value,
) -> Value {
    let mut packet = evidence_packet(
        format!(
            "{:?} `{}` is declared in `{}`",
            symbol.kind, symbol.name, symbol.file_id.0
        ),
        vec![span_for_path_json(&symbol.file_id.0, Some(symbol.span))],
        symbol.confidence,
        symbol.freshness,
        vec![symbol.provenance.clone()],
        ToolCostHint {
            matches_returned: 1,
            ..ToolCostHint::default()
        },
        next_action,
    );
    if let Some(object) = packet.as_object_mut() {
        object.insert("tool".to_string(), json!(tool));
        object.insert("symbol".to_string(), symbol_json(graph, symbol));
    }
    packet
}

fn symbol_next_action(symbol: &GraphSymbol) -> Value {
    json!({
        "tool": "symbol_context",
        "arguments": {
            "query": symbol.name,
            "path": symbol.file_id.0
        },
        "reason": "expand this declaration with callers and references"
    })
}

fn symbol_context_packet(
    graph: &squeezy_graph::SemanticGraph,
    symbol: &GraphSymbol,
    max_references: usize,
) -> Value {
    let mut packet = symbol_packet(
        graph,
        symbol,
        "symbol_context",
        json!({
            "tool": "read_slice",
            "arguments": {
                "symbol_id": symbol.id.0,
                "span_kind": "body"
            },
            "reason": "read the exact symbol body if details are needed"
        }),
    );
    if let Some(object) = packet.as_object_mut() {
        object.insert(
            "references".to_string(),
            json!(
                graph
                    .references_to_symbol(&symbol.id)
                    .into_iter()
                    .take(max_references)
                    .map(reference_json)
                    .collect::<Vec<_>>()
            ),
        );
        object.insert(
            "callers".to_string(),
            json!(
                graph
                    .callers(&symbol.id)
                    .into_iter()
                    .take(max_references)
                    .filter_map(|hit| hit.caller)
                    .map(|caller| symbol_summary_json(&caller))
                    .collect::<Vec<_>>()
            ),
        );
        object.insert(
            "callees".to_string(),
            json!(
                graph
                    .callees(&symbol.id)
                    .into_iter()
                    .take(max_references)
                    .filter_map(|hit| hit.callee)
                    .map(|callee| symbol_summary_json(&callee))
                    .collect::<Vec<_>>()
            ),
        );
        object.insert(
            "diagnostics".to_string(),
            json!(
                graph
                    .cargo_diagnostics_for_symbol(symbol)
                    .into_iter()
                    .take(max_references)
                    .map(|hit| cargo_diagnostic_hit_json(&hit))
                    .collect::<Vec<_>>()
            ),
        );
    }
    packet
}

fn reference_packet(hit: &ReferenceHit) -> Value {
    let mut packet = evidence_packet(
        format!(
            "reference `{}` appears in `{}`",
            hit.reference.text, hit.reference.file_id.0
        ),
        vec![span_for_path_json(
            &hit.reference.file_id.0,
            Some(hit.reference.span),
        )],
        hit.confidence,
        Freshness::Fresh,
        vec![hit.reference.provenance.clone()],
        ToolCostHint {
            matches_returned: 1,
            ..ToolCostHint::default()
        },
        json!({
            "tool": "read_slice",
            "arguments": {
                "path": hit.reference.file_id.0,
                "start_byte": hit.reference.span.start_byte,
                "end_byte": hit.reference.span.end_byte
            },
            "reason": "read the exact reference slice"
        }),
    );
    if let Some(object) = packet.as_object_mut() {
        object.insert("reference".to_string(), reference_json(hit.clone()));
    }
    packet
}

fn reference_matches_path(hit: &ReferenceHit, filter: &str) -> bool {
    let path = hit.reference.file_id.0.as_str();
    path == filter || path.ends_with(&format!("/{filter}"))
}

fn call_edge_packet(
    graph: &squeezy_graph::SemanticGraph,
    hit: &CallEdgeHit,
    tool: &str,
    upstream: bool,
) -> Value {
    let actor = if upstream {
        hit.caller.as_ref()
    } else {
        hit.callee.as_ref()
    };
    let claim = if upstream {
        format!(
            "`{}` calls `{}`",
            actor
                .map(|symbol| symbol.name.as_str())
                .unwrap_or("<unknown>"),
            hit.callee
                .as_ref()
                .map(|symbol| symbol.name.as_str())
                .unwrap_or(hit.edge.target_text.as_str())
        )
    } else {
        format!(
            "`{}` calls `{}`",
            hit.caller
                .as_ref()
                .map(|symbol| symbol.name.as_str())
                .unwrap_or("<unknown>"),
            actor
                .map(|symbol| symbol.name.as_str())
                .unwrap_or(hit.edge.target_text.as_str())
        )
    };
    let span = hit.edge.span.map(|span| {
        span_for_path_json(
            hit.caller
                .as_ref()
                .map(|symbol| symbol.file_id.0.as_str())
                .unwrap_or(""),
            Some(span),
        )
    });
    let next_action = if hit.edge.candidates.is_empty() {
        json!({
            "tool": "read_slice",
            "arguments": hit.edge.span.map(|span| json!({
                "path": hit.caller.as_ref().map(|symbol| symbol.file_id.0.clone()).unwrap_or_default(),
                "start_byte": span.start_byte,
                "end_byte": span.end_byte
            })).unwrap_or_else(|| json!({})),
            "reason": "read the exact call site"
        })
    } else {
        let fanout: Vec<Value> = hit
            .edge
            .candidates
            .iter()
            .take(CANDIDATE_FANOUT_LIMIT)
            .filter_map(|id| graph.symbols.get(id))
            .map(|sym| {
                json!({
                    "tool": "read_slice",
                    "arguments": {
                        "path": sym.file_id.0,
                        "start_byte": sym.span.start_byte,
                        "end_byte": sym.span.end_byte,
                    },
                    "symbol_id": sym.id.0,
                })
            })
            .collect();
        json!({
            "tool": "read_slice",
            "fanout": fanout,
            "reason": "candidate set: read each candidate's declaration to disambiguate"
        })
    };
    let mut packet = evidence_packet(
        claim,
        span.into_iter().collect(),
        hit.edge.confidence,
        hit.edge.freshness,
        vec![hit.edge.provenance.clone()],
        ToolCostHint {
            matches_returned: 1,
            ..ToolCostHint::default()
        },
        next_action,
    );
    if let Some(object) = packet.as_object_mut() {
        object.insert("tool".to_string(), json!(tool));
        object.insert("edge".to_string(), edge_json(&hit.edge));
        object.insert(
            "caller".to_string(),
            json!(hit.caller.as_ref().map(symbol_summary_json)),
        );
        object.insert(
            "callee".to_string(),
            json!(hit.callee.as_ref().map(symbol_summary_json)),
        );
        if !hit.edge.candidates.is_empty() {
            let candidate_objects: Vec<Value> = hit
                .edge
                .candidates
                .iter()
                .filter_map(|id| graph.symbols.get(id))
                .map(symbol_summary_json)
                .collect();
            object.insert("candidates".to_string(), json!(candidate_objects));
        }
    }
    packet
}

/// Maximum number of candidate symbols that get a dedicated `read_slice`
/// entry in the `fanout` next-action. The full list is preserved in the
/// `candidates` field of the packet.
const CANDIDATE_FANOUT_LIMIT: usize = 4;

#[derive(Debug, Clone, Copy)]
enum CallDirection {
    Upstream,
    Downstream,
}

impl CallDirection {
    fn tool(self) -> &'static str {
        match self {
            CallDirection::Upstream => "upstream_flow",
            CallDirection::Downstream => "downstream_flow",
        }
    }

    fn is_upstream(self) -> bool {
        matches!(self, CallDirection::Upstream)
    }

    fn neighbors(
        self,
        graph: &squeezy_graph::SemanticGraph,
        symbol_id: &SymbolId,
    ) -> Vec<CallEdgeHit> {
        match self {
            CallDirection::Upstream => graph.callers(symbol_id),
            CallDirection::Downstream => graph.callees(symbol_id),
        }
    }

    fn next_id(self, hit: &CallEdgeHit) -> Option<SymbolId> {
        match self {
            CallDirection::Upstream => hit.caller.as_ref().map(|symbol| symbol.id.clone()),
            CallDirection::Downstream => hit.callee.as_ref().map(|symbol| symbol.id.clone()),
        }
    }
}

struct CallTraversal {
    packets: Vec<Value>,
    overflowed: bool,
}

/// Bounded BFS over `callers`/`callees`. Each emitted packet carries the
/// hop distance from `root` (1-indexed) so the model can interpret a flat
/// list of edges as a graph. Recursion is gated by `visited` to keep cyclic
/// call graphs from looping; every edge still emits a packet on first
/// encounter so the model sees the relationship even when expansion is
/// blocked. `overflowed` is true when the traversal had to stop before
/// reaching all in-budget neighbors (either we hit `max_results`, or we hit
/// `max_depth` with more frontier nodes still expandable).
fn bfs_call_packets(
    graph: &squeezy_graph::SemanticGraph,
    root: &GraphSymbol,
    max_depth: usize,
    max_results: usize,
    direction: CallDirection,
) -> CallTraversal {
    let mut packets = Vec::new();
    if max_results == 0 || max_depth == 0 {
        let overflowed = !direction.neighbors(graph, &root.id).is_empty();
        return CallTraversal {
            packets,
            overflowed,
        };
    }
    let mut visited: HashSet<SymbolId> = HashSet::from([root.id.clone()]);
    let mut frontier: VecDeque<(SymbolId, usize)> = VecDeque::from([(root.id.clone(), 0usize)]);
    let mut overflowed = false;
    while let Some((current_id, depth)) = frontier.pop_front() {
        if depth >= max_depth {
            continue;
        }
        let next_depth = depth + 1;
        for hit in direction.neighbors(graph, &current_id) {
            if packets.len() >= max_results {
                overflowed = true;
                return CallTraversal {
                    packets,
                    overflowed,
                };
            }
            let mut packet =
                call_edge_packet(graph, &hit, direction.tool(), direction.is_upstream());
            if let Some(object) = packet.as_object_mut() {
                object.insert("depth".to_string(), json!(next_depth));
            }
            packets.push(packet);
            if next_depth < max_depth
                && let Some(next_id) = direction.next_id(&hit)
                && visited.insert(next_id.clone())
            {
                frontier.push_back((next_id, next_depth));
            }
        }
    }
    CallTraversal {
        packets,
        overflowed,
    }
}

fn edge_packet(graph: &squeezy_graph::SemanticGraph, edge: &GraphEdge, tool: &str) -> Value {
    let from = graph.symbols.get(&edge.from);
    let to = edge.to.as_ref().and_then(|id| graph.symbols.get(id));
    let span = edge.span.map(|span| {
        span_for_path_json(
            from.map(|symbol| symbol.file_id.0.as_str()).unwrap_or(""),
            Some(span),
        )
    });
    let mut packet = evidence_packet(
        format!(
            "`{}` has {:?} edge to `{}`",
            from.map(|symbol| symbol.name.as_str())
                .unwrap_or(&edge.from.0),
            edge.kind,
            to.map(|symbol| symbol.name.as_str())
                .unwrap_or(edge.target_text.as_str())
        ),
        span.into_iter().collect(),
        edge.confidence,
        edge.freshness,
        vec![edge.provenance.clone()],
        ToolCostHint {
            matches_returned: 1,
            ..ToolCostHint::default()
        },
        json!({
            "tool": "symbol_context",
            "arguments": {
                "query": edge.target_text
            },
            "reason": "inspect the target symbol context"
        }),
    );
    if let Some(object) = packet.as_object_mut() {
        object.insert("tool".to_string(), json!(tool));
        object.insert("edge".to_string(), edge_json(edge));
    }
    packet
}

fn call_chain_packet(
    graph: &squeezy_graph::SemanticGraph,
    chain: &[SymbolId],
    source: &GraphSymbol,
    target: &GraphSymbol,
) -> Value {
    let symbols = chain
        .iter()
        .filter_map(|id| graph.symbols.get(id))
        .cloned()
        .collect::<Vec<_>>();
    let claim = format!(
        "call chain found: {}",
        symbols
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>()
            .join(" -> ")
    );
    let mut packet = evidence_packet(
        claim,
        symbols
            .iter()
            .map(|symbol| span_for_path_json(&symbol.file_id.0, Some(symbol.span)))
            .collect(),
        Confidence::Heuristic,
        Freshness::Fresh,
        vec![Provenance::new(
            "squeezy-graph",
            "bounded call_chain traversal over resolved call edges",
        )],
        ToolCostHint {
            matches_returned: symbols.len() as u64,
            ..ToolCostHint::default()
        },
        json!({
            "tool": "symbol_context",
            "arguments": {
                "symbol_id": target.id.0,
                "query": target.name
            },
            "reason": "inspect the target at the end of the chain"
        }),
    );
    if let Some(object) = packet.as_object_mut() {
        object.insert("source".to_string(), symbol_json(graph, source));
        object.insert("target".to_string(), symbol_json(graph, target));
        object.insert(
            "chain".to_string(),
            json!(symbols.iter().map(symbol_summary_json).collect::<Vec<_>>()),
        );
    }
    packet
}

fn hierarchy_node_packet(
    graph: &squeezy_graph::SemanticGraph,
    node: &HierarchyNode,
    tool: &str,
) -> Value {
    if let Some(symbol) = graph.symbols.get(&node.id) {
        return symbol_packet(graph, symbol, tool, symbol_next_action(symbol));
    }
    evidence_packet(
        format!("{:?} `{}` appears in hierarchy", node.kind, node.name),
        vec![span_for_path_json(&node.name, Some(node.span))],
        Confidence::ExactSyntax,
        node.freshness,
        vec![Provenance::new(
            "squeezy-graph",
            "hierarchy traversal result",
        )],
        ToolCostHint {
            matches_returned: 1,
            ..ToolCostHint::default()
        },
        json!({
            "tool": "hierarchy",
            "arguments": {"symbol_id": node.id.0},
            "reason": "expand this hierarchy node"
        }),
    )
}

pub(crate) fn symbol_json(graph: &squeezy_graph::SemanticGraph, symbol: &GraphSymbol) -> Value {
    json!({
        "id": symbol.id.0,
        "name": symbol.name,
        "kind": format!("{:?}", symbol.kind),
        "path": symbol.file_id.0,
        "language": graph.files.get(&symbol.file_id).map(|file| file.language.display_name()),
        "signature": symbol.signature,
        "visibility": symbol.visibility,
        "span": span_json(symbol.span),
        "body_span": symbol.body_span.map(span_json),
        "attributes": symbol.attributes,
        "dirty": symbol.dirty.as_ref().map(|dirty| json!({
            "status": dirty.status,
            "ranges": dirty.ranges.iter().map(|range| json!({
                "start_line": range.start_line,
                "end_line": range.end_line,
            })).collect::<Vec<_>>(),
        })),
        "confidence": symbol.confidence.id(),
        "freshness": format!("{:?}", symbol.freshness),
    })
}

pub(crate) fn symbol_summary_json(symbol: &GraphSymbol) -> Value {
    json!({
        "id": symbol.id.0,
        "name": symbol.name,
        "kind": format!("{:?}", symbol.kind),
        "path": symbol.file_id.0,
        "span": span_json(symbol.span),
    })
}

fn edge_json(edge: &GraphEdge) -> Value {
    let mut value = json!({
        "from": edge.from.0,
        "to": edge.to.as_ref().map(|id| id.0.clone()),
        "target_text": edge.target_text,
        "kind": format!("{:?}", edge.kind),
        "span": edge.span.map(span_json),
        "confidence": edge.confidence.id(),
        "freshness": format!("{:?}", edge.freshness),
        "provenance": provenance_json(edge.provenance.clone()),
    });
    if !edge.candidates.is_empty()
        && let Some(object) = value.as_object_mut()
    {
        object.insert(
            "candidates".to_string(),
            json!(
                edge.candidates
                    .iter()
                    .map(|id| id.0.clone())
                    .collect::<Vec<_>>()
            ),
        );
    }
    value
}

fn hierarchy_node_json(graph: &squeezy_graph::SemanticGraph, node: &HierarchyNode) -> Value {
    json!({
        "id": node.id.0,
        "name": node.name,
        "kind": format!("{:?}", node.kind),
        "span": span_json(node.span),
        "freshness": format!("{:?}", node.freshness),
        "symbol": graph.symbols.get(&node.id).map(symbol_summary_json),
        "children": node.children.iter().map(|child| hierarchy_node_json(graph, child)).collect::<Vec<_>>(),
    })
}

#[allow(clippy::too_many_arguments)]
fn hierarchy_result(
    call: &ToolCall,
    manager: &GraphManager,
    refresh: &squeezy_graph::RefreshReport,
    graph: &squeezy_graph::SemanticGraph,
    nodes: Vec<HierarchyNode>,
    max_depth: usize,
    max_results: Option<usize>,
    root: Option<GraphSymbol>,
) -> ToolResult {
    let max_results = graph_limit(max_results);
    let truncated = nodes.len() > max_results;
    let selected = nodes.iter().take(max_results).collect::<Vec<_>>();
    let hierarchy = selected
        .iter()
        .map(|node| hierarchy_node_json(graph, node))
        .collect::<Vec<_>>();
    let packets = selected
        .iter()
        .map(|node| hierarchy_node_packet(graph, node, "hierarchy"))
        .collect::<Vec<_>>();
    let mut payload = graph_payload("hierarchy", manager, refresh);
    payload.insert("max_depth".to_string(), json!(max_depth));
    payload.insert(
        "root".to_string(),
        json!(root.as_ref().map(|symbol| symbol_json(graph, symbol))),
    );
    payload.insert("hierarchy".to_string(), json!(hierarchy));
    payload.insert("packets".to_string(), json!(packets));
    payload.insert("truncated".to_string(), json!(truncated));
    make_result(
        call,
        ToolStatus::Success,
        Value::Object(payload),
        ToolCostHint {
            matches_returned: selected.len() as u64,
            truncated,
            ..ToolCostHint::default()
        },
        None,
    )
}

fn resolve_single_symbol(
    graph: &squeezy_graph::SemanticGraph,
    args: &FlowArgs,
) -> Option<GraphSymbol> {
    if let Some(symbol_id) = args.symbol_id.as_deref() {
        return graph.symbols.get(&SymbolId::new(symbol_id)).cloned();
    }
    let query = args.query.as_deref()?;
    graph_symbol_search(
        graph,
        Some(query),
        args.kind.as_deref(),
        args.path.as_deref(),
        None,
        None,
        None,
    )
    .into_iter()
    .next()
}

fn resolve_flow_target(
    graph: &squeezy_graph::SemanticGraph,
    args: &FlowArgs,
) -> Option<GraphSymbol> {
    if let Some(symbol_id) = args.target_symbol_id.as_deref() {
        return graph.symbols.get(&SymbolId::new(symbol_id)).cloned();
    }
    let query = args.target_query.as_deref()?;
    graph_symbol_search(graph, Some(query), None, None, None, None, None)
        .into_iter()
        .next()
}

fn unresolved_symbol_result(
    call: &ToolCall,
    tool: &str,
    manager: &GraphManager,
    refresh: &squeezy_graph::RefreshReport,
    args: &FlowArgs,
) -> ToolResult {
    let graph = manager.graph();
    let query = args.query.as_deref().unwrap_or("");
    let candidates = if query.is_empty() {
        Vec::new()
    } else {
        graph_symbol_search(
            graph,
            Some(query),
            args.kind.as_deref(),
            args.path.as_deref(),
            None,
            None,
            None,
        )
    };
    let packets = candidates
        .iter()
        .take(DEFAULT_GRAPH_MAX_RESULTS)
        .map(|symbol| symbol_packet(graph, symbol, tool, symbol_next_action(symbol)))
        .collect::<Vec<_>>();
    let mut payload = graph_payload(tool, manager, refresh);
    payload.insert("resolved".to_string(), json!(false));
    payload.insert("symbol_id".to_string(), json!(args.symbol_id));
    payload.insert("query".to_string(), json!(args.query));
    payload.insert("packets".to_string(), json!(packets));
    payload.insert(
        "next_action".to_string(),
        json!({
            "tool": "definition_search",
            "arguments": {"query": query},
            "reason": "resolve a unique symbol before asking for flow"
        }),
    );
    make_result(
        call,
        ToolStatus::Stale,
        Value::Object(payload),
        ToolCostHint {
            matches_returned: candidates.len().min(DEFAULT_GRAPH_MAX_RESULTS) as u64,
            truncated: candidates.len() > DEFAULT_GRAPH_MAX_RESULTS,
            ..ToolCostHint::default()
        },
        None,
    )
}

fn resolve_hierarchy_root(
    graph: &squeezy_graph::SemanticGraph,
    args: &HierarchyArgs,
) -> Option<GraphSymbol> {
    if let Some(symbol_id) = args.symbol_id.as_deref() {
        return graph.symbols.get(&SymbolId::new(symbol_id)).cloned();
    }
    let query = args.query.as_deref()?;
    graph_symbol_search(
        graph,
        Some(query),
        args.kind.as_deref(),
        args.path.as_deref(),
        None,
        None,
        None,
    )
    .into_iter()
    .next()
}

fn unresolved_hierarchy_result(
    call: &ToolCall,
    manager: &GraphManager,
    refresh: &squeezy_graph::RefreshReport,
    args: &HierarchyArgs,
) -> ToolResult {
    let graph = manager.graph();
    let query = args.query.as_deref().unwrap_or("");
    let packets = if query.is_empty() {
        Vec::new()
    } else {
        graph_symbol_search(
            graph,
            Some(query),
            args.kind.as_deref(),
            args.path.as_deref(),
            None,
            None,
            None,
        )
        .into_iter()
        .take(DEFAULT_GRAPH_MAX_RESULTS)
        .map(|symbol| symbol_packet(graph, &symbol, "hierarchy", symbol_next_action(&symbol)))
        .collect::<Vec<_>>()
    };
    let mut payload = graph_payload("hierarchy", manager, refresh);
    payload.insert("resolved".to_string(), json!(false));
    payload.insert("symbol_id".to_string(), json!(args.symbol_id));
    payload.insert("query".to_string(), json!(args.query));
    payload.insert("packets".to_string(), json!(packets));
    make_result(
        call,
        ToolStatus::Stale,
        Value::Object(payload),
        ToolCostHint::default(),
        None,
    )
}

type ReadSliceTarget = (
    String,
    Option<SourceSpan>,
    &'static str,
    Confidence,
    Vec<Provenance>,
);

fn read_slice_target(
    graph: Option<&squeezy_graph::SemanticGraph>,
    args: &ReadSliceArgs,
) -> std::result::Result<ReadSliceTarget, String> {
    if let Some(symbol_id) = args.symbol_id.as_deref() {
        let graph =
            graph.ok_or_else(|| "read_slice symbol_id requires an available graph".to_string())?;
        let symbol = graph
            .symbols
            .get(&SymbolId::new(symbol_id))
            .ok_or_else(|| format!("symbol_id not found: {symbol_id}"))?;
        let span = match args.span_kind.unwrap_or_default() {
            ReadSliceSpanKind::Signature => symbol.span,
            ReadSliceSpanKind::Body => symbol.body_span.unwrap_or(symbol.span),
        };
        return Ok((
            symbol.file_id.0.clone(),
            Some(span),
            "graph_symbol",
            symbol.confidence,
            vec![symbol.provenance.clone()],
        ));
    }
    let path = args
        .path
        .clone()
        .ok_or_else(|| "read_slice requires path or symbol_id".to_string())?;
    let status = graph
        .and_then(|graph| {
            graph.files.values().find(|file| {
                file.relative_path == path || file.relative_path.ends_with(&format!("/{path}"))
            })
        })
        .map(|file| graph_status_for_language(file.language))
        .unwrap_or("not_indexed");
    Ok((
        path,
        None,
        status,
        // Path-only reads pick bytes the caller asked for, not bytes that came
        // from a tree-sitter span. Don't claim `ExactSyntax` confidence here:
        // the bytes are exactly what was requested, but their relationship to
        // the surrounding syntax is the caller's assertion, not the graph's.
        Confidence::Heuristic,
        vec![Provenance::new(
            "squeezy-tools",
            "explicit bounded file slice",
        )],
    ))
}

fn read_slice_byte_window(
    path: &Path,
    total_bytes: u64,
    args: &ReadSliceArgs,
    symbol_span: Option<SourceSpan>,
) -> std::result::Result<(usize, usize, Option<SourceSpan>), String> {
    if let Some(span) = symbol_span {
        let start = span.start_byte as usize;
        let end = span.end_byte.max(span.start_byte) as usize;
        let limit = end.saturating_sub(start).clamp(1, MAX_READ_LIMIT);
        return Ok((start.min(total_bytes as usize), limit, Some(span)));
    }
    if args.start_line.is_some() || args.end_line.is_some() {
        if total_bytes > GRAPH_READ_SLICE_MAX_LINE_SCAN_BYTES {
            return Err(format!(
                "line-based read_slice refuses to scan files larger than {GRAPH_READ_SLICE_MAX_LINE_SCAN_BYTES} bytes; use start_byte/end_byte instead"
            ));
        }
        let bytes = fs::read(path).map_err(|err| err.to_string())?;
        let text = String::from_utf8_lossy(&bytes);
        let (start, end, span) = line_window(&text, args)?;
        let limit = end.saturating_sub(start).clamp(1, MAX_READ_LIMIT);
        return Ok((start, limit, Some(span)));
    }
    if let Some(start) = args.start_byte {
        let end = args
            .end_byte
            .unwrap_or_else(|| start.saturating_add(args.limit.unwrap_or(DEFAULT_READ_LIMIT)));
        let limit = end.saturating_sub(start).clamp(1, MAX_READ_LIMIT);
        return Ok((start.min(total_bytes as usize), limit, None));
    }
    let offset = args.offset.unwrap_or(0).min(total_bytes as usize);
    let limit = args.limit.unwrap_or(DEFAULT_READ_LIMIT).min(MAX_READ_LIMIT);
    Ok((offset, limit, None))
}

fn line_window(
    text: &str,
    args: &ReadSliceArgs,
) -> std::result::Result<(usize, usize, SourceSpan), String> {
    let total_lines = text.lines().count().max(1) as u32;
    let context = args.context_lines.unwrap_or(0);
    let start_line = args.start_line.unwrap_or(1).max(1).saturating_sub(context);
    let start_line = start_line.max(1);
    let end_line = args
        .end_line
        .unwrap_or(args.start_line.unwrap_or(total_lines))
        .saturating_add(context)
        .min(total_lines)
        .max(start_line);
    let mut byte = 0usize;
    let mut start_byte = None;
    let mut end_byte = None;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let line_no = index as u32 + 1;
        if line_no == start_line {
            start_byte = Some(byte);
        }
        byte += line.len();
        if line_no == end_line {
            end_byte = Some(byte);
            break;
        }
    }
    let start = start_byte.ok_or_else(|| "start_line is outside the file".to_string())?;
    let end = end_byte.unwrap_or(text.len());
    let span = SourceSpan::new(
        start as u32,
        end as u32,
        squeezy_core::SourcePoint::new(start_line.saturating_sub(1), 0),
        squeezy_core::SourcePoint::new(end_line.saturating_sub(1), 0),
    );
    Ok((start, end, span))
}

fn span_for_path_json(path: impl ToString, span: Option<SourceSpan>) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("path".to_string(), json!(path.to_string()));
    if let Some(span) = span {
        object.insert("span".to_string(), span_json(span));
    }
    Value::Object(object)
}

pub(crate) fn reference_json(hit: ReferenceHit) -> Value {
    json!({
        "path": hit.reference.file_id.0,
        "text": hit.reference.text,
        "kind": format!("{:?}", hit.reference.kind),
        "span": span_json(hit.reference.span),
        "owner": hit.owner.map(|owner| json!({
            "id": owner.id.0,
            "name": owner.name,
            "kind": format!("{:?}", owner.kind),
        })),
        "confidence": hit.confidence.id(),
    })
}

fn span_json(span: squeezy_core::SourceSpan) -> Value {
    json!({
        "start_byte": span.start_byte,
        "end_byte": span.end_byte,
        "start": {"line": span.start.line, "column": span.start.column},
        "end": {"line": span.end.line, "column": span.end.column},
    })
}

impl ToolRegistry {
    pub(crate) async fn execute_graph_tool(&self, call: &ToolCall) -> ToolResult {
        let registry = self.clone();
        let call = call.clone();
        // Preserve the original `call_id` and tool name so the agent loop can
        // still match a join failure back to the model's tool call and so
        // telemetry classifies it under the right tool family instead of a
        // generic `graph_tool` bucket.
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

    fn execute_graph_tool_blocking(&self, call: &ToolCall) -> ToolResult {
        let mode = graph_tool_diff_mode(call);
        let snapshot = self.diff_snapshot(mode, DiffOptions::default());
        self.wait_for_graph_ready(GRAPH_READY_WAIT);
        let mut graph = match self.graph.lock() {
            Ok(graph) => graph,
            Err(_) => {
                return make_result(
                    call,
                    ToolStatus::Error,
                    json!({"error": "semantic graph lock poisoned"}),
                    ToolCostHint::default(),
                    None,
                );
            }
        };
        let Some(manager) = graph.as_mut() else {
            return graph_unavailable_result(call);
        };
        let refresh = match manager.refresh_before_query() {
            Ok(report) => report,
            Err(err) => return tool_error(call, err),
        };
        annotate_graph(manager, &snapshot);
        let graph = manager.graph();

        match call.name.as_str() {
            "repo_map" => match serde_json::from_value::<RepoMapArgs>(call.arguments.clone()) {
                Ok(args) => self.execute_repo_map_blocking(call, args, manager, &refresh),
                Err(err) => tool_arg_error(call, err),
            },
            "decl_search" => match serde_json::from_value::<DeclSearchArgs>(call.arguments.clone())
            {
                Ok(args) => self.execute_decl_search_blocking(call, args, manager, &refresh),
                Err(err) => tool_arg_error(call, err),
            },
            "definition_search" => {
                match serde_json::from_value::<DefinitionSearchArgs>(call.arguments.clone()) {
                    Ok(args) => {
                        self.execute_definition_search_blocking(call, args, manager, &refresh)
                    }
                    Err(err) => tool_arg_error(call, err),
                }
            }
            "reference_search" => {
                match serde_json::from_value::<ReferenceSearchArgs>(call.arguments.clone()) {
                    Ok(args) => {
                        self.execute_reference_search_blocking(call, args, manager, &refresh)
                    }
                    Err(err) => tool_arg_error(call, err),
                }
            }
            "upstream_flow" => match serde_json::from_value::<FlowArgs>(call.arguments.clone()) {
                Ok(args) => self.execute_upstream_flow_blocking(call, args, manager, &refresh),
                Err(err) => tool_arg_error(call, err),
            },
            "downstream_flow" => match serde_json::from_value::<FlowArgs>(call.arguments.clone()) {
                Ok(args) => self.execute_downstream_flow_blocking(call, args, manager, &refresh),
                Err(err) => tool_arg_error(call, err),
            },
            "symbol_context" => {
                match serde_json::from_value::<SymbolContextArgs>(call.arguments.clone()) {
                    Ok(args) => self.execute_symbol_context_graph_blocking(
                        call, args, manager, &refresh, &snapshot,
                    ),
                    Err(err) => tool_arg_error(call, err),
                }
            }
            "hierarchy" => match serde_json::from_value::<HierarchyArgs>(call.arguments.clone()) {
                Ok(args) => self.execute_hierarchy_blocking(call, args, manager, &refresh),
                Err(err) => tool_arg_error(call, err),
            },
            "read_slice" => match serde_json::from_value::<ReadSliceArgs>(call.arguments.clone()) {
                Ok(args) => self.execute_read_slice_blocking(call, args, Some(graph)),
                Err(err) => tool_arg_error(call, err),
            },
            _ => make_result(
                call,
                ToolStatus::Error,
                json!({ "error": format!("unknown graph tool: {}", call.name) }),
                ToolCostHint::default(),
                None,
            ),
        }
    }

    fn execute_repo_map_blocking(
        &self,
        call: &ToolCall,
        args: RepoMapArgs,
        manager: &GraphManager,
        refresh: &squeezy_graph::RefreshReport,
    ) -> ToolResult {
        let graph = manager.graph();
        let max_depth = args.max_depth.unwrap_or(2).clamp(1, MAX_GRAPH_MAX_DEPTH);
        let max_files = args.max_files.unwrap_or(50).clamp(1, 200);
        let nodes = graph.hierarchy(None, max_depth);
        let truncated = nodes.len() > max_files;
        let selected = nodes.iter().take(max_files).collect::<Vec<_>>();
        let hierarchy = selected
            .iter()
            .map(|node| hierarchy_node_json(graph, node))
            .collect::<Vec<_>>();
        let packets = selected
            .iter()
            .map(|node| hierarchy_node_packet(graph, node, "repo_map"))
            .collect::<Vec<_>>();
        let unsupported = unsupported_file_samples(graph, 25);
        let mut payload = graph_payload("repo_map", manager, refresh);
        payload.insert("max_depth".to_string(), json!(max_depth));
        payload.insert("stats".to_string(), graph_stats_json(graph));
        payload.insert("languages".to_string(), graph_language_counts_json(graph));
        payload.insert("hierarchy".to_string(), json!(hierarchy));
        payload.insert("packets".to_string(), json!(packets));
        payload.insert("unsupported_files".to_string(), json!(unsupported));
        payload.insert("truncated".to_string(), json!(truncated));
        make_result(
            call,
            ToolStatus::Success,
            Value::Object(payload),
            ToolCostHint {
                matches_returned: selected.len() as u64,
                truncated,
                ..ToolCostHint::default()
            },
            None,
        )
    }

    fn execute_decl_search_blocking(
        &self,
        call: &ToolCall,
        args: DeclSearchArgs,
        manager: &GraphManager,
        refresh: &squeezy_graph::RefreshReport,
    ) -> ToolResult {
        let graph = manager.graph();
        if !decl_search_has_query_or_filter(&args) {
            return make_result(
                call,
                ToolStatus::Error,
                json!({
                    "error": "decl_search requires a query or at least one filter",
                    "retry": "provide query, kind, language, path, visibility, or attribute",
                }),
                ToolCostHint::default(),
                None,
            );
        }
        let max_results = graph_limit(args.max_results);
        let offset = args.offset.unwrap_or(0);
        let symbols = graph_symbol_search(
            graph,
            args.query.as_deref(),
            args.kind.as_deref(),
            args.path.as_deref(),
            args.language.as_deref(),
            args.visibility.as_deref(),
            args.attribute.as_deref(),
        );
        let truncated = symbols.len().saturating_sub(offset) > max_results;
        let selected = symbols
            .iter()
            .skip(offset)
            .take(max_results)
            .cloned()
            .collect::<Vec<_>>();
        let packets = selected
            .iter()
            .map(|symbol| symbol_packet(graph, symbol, "decl_search", symbol_next_action(symbol)))
            .collect::<Vec<_>>();
        let confidence_distribution =
            ToolCostHint::confidence_distribution_from(selected.iter().map(|s| s.confidence));
        let mut payload = graph_payload("decl_search", manager, refresh);
        payload.insert("query".to_string(), json!(args.query));
        payload.insert("kind".to_string(), json!(args.kind));
        payload.insert("language".to_string(), json!(args.language));
        let packet_count = packets.len();
        payload.insert("packets".to_string(), json!(packets));
        payload.insert(
            "fallback".to_string(),
            graph_zero_hit_fallback(
                graph,
                args.path.as_deref(),
                args.query.as_deref(),
                packet_count,
            ),
        );
        payload.insert("offset".to_string(), json!(offset));
        payload.insert("total_matches".to_string(), json!(symbols.len()));
        payload.insert("returned_matches".to_string(), json!(selected.len()));
        payload.insert(
            "counts_by_language".to_string(),
            decl_counts_by_language(graph, &symbols),
        );
        payload.insert("counts_by_kind".to_string(), decl_counts_by_kind(&symbols));
        payload.insert("truncated".to_string(), json!(truncated));
        make_result(
            call,
            ToolStatus::Success,
            Value::Object(payload),
            ToolCostHint {
                matches_returned: selected.len() as u64,
                truncated,
                confidence_distribution,
                ..ToolCostHint::default()
            },
            None,
        )
    }

    fn execute_definition_search_blocking(
        &self,
        call: &ToolCall,
        args: DefinitionSearchArgs,
        manager: &GraphManager,
        refresh: &squeezy_graph::RefreshReport,
    ) -> ToolResult {
        let graph = manager.graph();
        let max_results = graph_limit(args.max_results);
        let symbols = resolve_definition_candidates(
            graph,
            args.symbol_id.as_deref(),
            args.query.as_deref(),
            args.kind.as_deref(),
            args.path.as_deref(),
            args.language.as_deref(),
        );
        let truncated = symbols.len() > max_results;
        let selected = symbols.into_iter().take(max_results).collect::<Vec<_>>();
        let packets = selected
            .iter()
            .map(|symbol| {
                symbol_packet(
                    graph,
                    symbol,
                    "definition_search",
                    json!({
                        "tool": "read_slice",
                        "arguments": {
                            "symbol_id": symbol.id.0,
                            "span_kind": "signature"
                        },
                        "reason": "read the exact declaration slice"
                    }),
                )
            })
            .collect::<Vec<_>>();
        let packet_count = packets.len();
        let mut payload = graph_payload("definition_search", manager, refresh);
        payload.insert("query".to_string(), json!(args.query));
        payload.insert("symbol_id".to_string(), json!(args.symbol_id));
        payload.insert("packets".to_string(), json!(packets));
        payload.insert(
            "fallback".to_string(),
            graph_zero_hit_fallback(
                graph,
                args.path.as_deref(),
                args.query.as_deref(),
                packet_count,
            ),
        );
        payload.insert("truncated".to_string(), json!(truncated));
        make_result(
            call,
            ToolStatus::Success,
            Value::Object(payload),
            ToolCostHint {
                matches_returned: selected.len() as u64,
                truncated,
                ..ToolCostHint::default()
            },
            None,
        )
    }

    fn execute_reference_search_blocking(
        &self,
        call: &ToolCall,
        args: ReferenceSearchArgs,
        manager: &GraphManager,
        refresh: &squeezy_graph::RefreshReport,
    ) -> ToolResult {
        let graph = manager.graph();
        let max_results = graph_limit(args.max_results);
        let offset = args.offset.unwrap_or(0);
        let hits = if let Some(symbol_id) = args.symbol_id.as_deref() {
            graph.references_to_symbol(&SymbolId::new(symbol_id))
        } else if let Some(text) = args.text.as_deref().or(args.query.as_deref()) {
            graph.reference_search(text)
        } else {
            return make_result(
                call,
                ToolStatus::Error,
                json!({ "error": "reference_search requires symbol_id, text, or query" }),
                ToolCostHint::default(),
                None,
            );
        };
        let filtered = hits
            .into_iter()
            .filter(|hit| {
                args.path
                    .as_deref()
                    .map(|path| reference_matches_path(hit, path))
                    .unwrap_or(true)
            })
            .collect::<Vec<_>>();
        let truncated = filtered.len().saturating_sub(offset) > max_results;
        let selected = filtered
            .iter()
            .skip(offset)
            .take(max_results)
            .cloned()
            .collect::<Vec<_>>();
        let packets = selected.iter().map(reference_packet).collect::<Vec<_>>();
        let confidence_distribution =
            ToolCostHint::confidence_distribution_from(selected.iter().map(|hit| hit.confidence));
        let query_text = args.text.clone().or_else(|| args.query.clone());
        let packet_count = packets.len();
        let mut payload = graph_payload("reference_search", manager, refresh);
        payload.insert("symbol_id".to_string(), json!(args.symbol_id));
        payload.insert("text".to_string(), json!(args.text.or(args.query)));
        payload.insert("packets".to_string(), json!(packets));
        payload.insert(
            "fallback".to_string(),
            graph_zero_hit_fallback(
                graph,
                args.path.as_deref(),
                query_text.as_deref(),
                packet_count,
            ),
        );
        payload.insert("offset".to_string(), json!(offset));
        payload.insert("truncated".to_string(), json!(truncated));
        make_result(
            call,
            ToolStatus::Success,
            Value::Object(payload),
            ToolCostHint {
                matches_returned: selected.len() as u64,
                truncated,
                confidence_distribution,
                ..ToolCostHint::default()
            },
            None,
        )
    }

    fn execute_upstream_flow_blocking(
        &self,
        call: &ToolCall,
        args: FlowArgs,
        manager: &GraphManager,
        refresh: &squeezy_graph::RefreshReport,
    ) -> ToolResult {
        let graph = manager.graph();
        let Some(symbol) = resolve_single_symbol(graph, &args) else {
            return unresolved_symbol_result(call, "upstream_flow", manager, refresh, &args);
        };
        let max_results = graph_limit(args.max_results);
        let max_depth = args
            .max_depth
            .unwrap_or(DEFAULT_GRAPH_MAX_DEPTH)
            .clamp(1, MAX_GRAPH_MAX_DEPTH);
        let traversal = bfs_call_packets(
            graph,
            &symbol,
            max_depth,
            max_results,
            CallDirection::Upstream,
        );
        let mut packets = traversal.packets;
        let mut overflowed = traversal.overflowed;
        if packets.len() < max_results {
            let inbound = graph.references_to_symbol(&symbol.id);
            let remaining = max_results - packets.len();
            if inbound.len() > remaining {
                overflowed = true;
            }
            for hit in inbound.into_iter().take(remaining) {
                packets.push(reference_packet(&hit));
            }
        }
        let truncated = overflowed;
        let confidence_distribution = ToolCostHint::confidence_distribution_from_packets(&packets);
        let mut payload = graph_payload("upstream_flow", manager, refresh);
        payload.insert("symbol".to_string(), symbol_json(graph, &symbol));
        payload.insert("max_depth".to_string(), json!(max_depth));
        let packet_count = packets.len();
        payload.insert("packets".to_string(), json!(packets));
        payload.insert("truncated".to_string(), json!(truncated));
        make_result(
            call,
            ToolStatus::Success,
            Value::Object(payload),
            ToolCostHint {
                matches_returned: packet_count as u64,
                truncated,
                confidence_distribution,
                ..ToolCostHint::default()
            },
            None,
        )
    }

    fn execute_downstream_flow_blocking(
        &self,
        call: &ToolCall,
        args: FlowArgs,
        manager: &GraphManager,
        refresh: &squeezy_graph::RefreshReport,
    ) -> ToolResult {
        let graph = manager.graph();
        let Some(symbol) = resolve_single_symbol(graph, &args) else {
            return unresolved_symbol_result(call, "downstream_flow", manager, refresh, &args);
        };
        let max_results = graph_limit(args.max_results);
        let max_depth = args
            .max_depth
            .unwrap_or(DEFAULT_GRAPH_MAX_DEPTH)
            .clamp(1, MAX_GRAPH_MAX_DEPTH);
        let mut packets = Vec::new();
        // Explicit call_chain ("does source eventually reach target?") goes
        // first so the model sees the directed answer before the broader BFS
        // listing of callees.
        if let Some(target) = resolve_flow_target(graph, &args)
            && let Some(chain) = graph.call_chain(&symbol.id, &target.id, max_depth)
        {
            packets.push(call_chain_packet(graph, &chain, &symbol, &target));
        }
        let traversal = bfs_call_packets(
            graph,
            &symbol,
            max_depth,
            max_results.saturating_sub(packets.len()),
            CallDirection::Downstream,
        );
        let mut overflowed = traversal.overflowed;
        packets.extend(traversal.packets);
        if packets.len() < max_results {
            let outgoing = graph
                .edges()
                .iter()
                .filter(|edge| edge.from == symbol.id)
                .filter(|edge| {
                    matches!(
                        edge.kind,
                        EdgeKind::Imports | EdgeKind::Reexports | EdgeKind::References
                    )
                })
                .collect::<Vec<_>>();
            let remaining = max_results - packets.len();
            if outgoing.len() > remaining {
                overflowed = true;
            }
            for edge in outgoing.into_iter().take(remaining) {
                packets.push(edge_packet(graph, edge, "downstream_flow"));
            }
        }
        let truncated = overflowed;
        let confidence_distribution = ToolCostHint::confidence_distribution_from_packets(&packets);
        let mut payload = graph_payload("downstream_flow", manager, refresh);
        payload.insert("symbol".to_string(), symbol_json(graph, &symbol));
        payload.insert("max_depth".to_string(), json!(max_depth));
        let packet_count = packets.len();
        payload.insert("packets".to_string(), json!(packets));
        payload.insert("truncated".to_string(), json!(truncated));
        make_result(
            call,
            ToolStatus::Success,
            Value::Object(payload),
            ToolCostHint {
                matches_returned: packet_count as u64,
                truncated,
                confidence_distribution,
                ..ToolCostHint::default()
            },
            None,
        )
    }

    fn execute_symbol_context_graph_blocking(
        &self,
        call: &ToolCall,
        args: SymbolContextArgs,
        manager: &GraphManager,
        refresh: &squeezy_graph::RefreshReport,
        snapshot: &DiffSnapshot,
    ) -> ToolResult {
        let graph = manager.graph();
        let dirty_paths = diff_path_set(snapshot);
        let max_references = args.max_references.unwrap_or(12).min(50);
        let max_results = graph_limit(args.max_results);
        let path_filter = args.path.as_deref();
        let diff_only = args.diff_only.unwrap_or(false);
        let mut symbols = graph_symbol_search(
            graph,
            Some(&args.query),
            None,
            path_filter,
            None,
            None,
            None,
        )
        .into_iter()
        .filter(|symbol| {
            !diff_only || symbol.dirty.is_some() || dirty_paths.contains(&symbol.file_id.0)
        })
        .take(max_results)
        .collect::<Vec<_>>();
        if symbols.is_empty() && diff_only {
            symbols = graph
                .dirty_symbols()
                .into_iter()
                .filter(|symbol| symbol_matches_path_filter(symbol, path_filter))
                .filter(|symbol| {
                    symbol.name.contains(&args.query) || symbol.signature.contains(&args.query)
                })
                .take(max_results)
                .collect();
        }
        let packets = symbols
            .iter()
            .map(|symbol| symbol_context_packet(graph, symbol, max_references))
            .collect::<Vec<_>>();
        let confidence_distribution =
            ToolCostHint::confidence_distribution_from(symbols.iter().map(|s| s.confidence));
        let mut payload = graph_payload("symbol_context", manager, refresh);
        payload.insert("query".to_string(), json!(args.query));
        payload.insert(
            "mode".to_string(),
            json!(diff_mode_str(args.mode.unwrap_or_default())),
        );
        payload.insert("diff_only".to_string(), json!(diff_only));
        let packet_count = packets.len();
        payload.insert("packets".to_string(), json!(packets));
        payload.insert(
            "fallback".to_string(),
            graph_zero_hit_fallback(graph, path_filter, Some(&args.query), packet_count),
        );
        payload.insert("truncated".to_string(), json!(false));
        make_result(
            call,
            ToolStatus::Success,
            Value::Object(payload),
            ToolCostHint {
                matches_returned: symbols.len() as u64,
                confidence_distribution,
                ..ToolCostHint::default()
            },
            None,
        )
    }

    fn execute_hierarchy_blocking(
        &self,
        call: &ToolCall,
        args: HierarchyArgs,
        manager: &GraphManager,
        refresh: &squeezy_graph::RefreshReport,
    ) -> ToolResult {
        let graph = manager.graph();
        let max_depth = args
            .max_depth
            .unwrap_or(DEFAULT_GRAPH_MAX_DEPTH)
            .clamp(1, MAX_GRAPH_MAX_DEPTH);
        let root = resolve_hierarchy_root(graph, &args);
        if args.symbol_id.is_some() || args.query.is_some() {
            let Some(root) = root else {
                return unresolved_hierarchy_result(call, manager, refresh, &args);
            };
            let nodes = graph.hierarchy(Some(&root.id), max_depth);
            return hierarchy_result(
                call,
                manager,
                refresh,
                graph,
                nodes,
                max_depth,
                args.max_results,
                Some(root),
            );
        }
        let nodes = graph.hierarchy(None, max_depth);
        hierarchy_result(
            call,
            manager,
            refresh,
            graph,
            nodes,
            max_depth,
            args.max_results,
            None,
        )
    }

    fn execute_read_slice_blocking(
        &self,
        call: &ToolCall,
        args: ReadSliceArgs,
        graph: Option<&squeezy_graph::SemanticGraph>,
    ) -> ToolResult {
        let (path_arg, span, graph_status, confidence, provenance) =
            match read_slice_target(graph, &args) {
                Ok(target) => target,
                Err(err) => return tool_error(call, err),
            };
        let diff_mode = args.read_mode.unwrap_or_default() == ReadSliceReadMode::Diff;
        let path = if diff_mode {
            match self.join_workspace(&path_arg) {
                Ok(path) => path,
                Err(err) => return tool_error(call, err),
            }
        } else {
            match self.resolve_existing(&path_arg) {
                Ok(path) => path,
                Err(err) => return tool_error(call, err),
            }
        };
        let rel = self.relative(&path);
        let rel_str = workspace_path(&rel);
        if !diff_mode && args.diff_only.unwrap_or(false) {
            let diff_paths =
                diff_path_set(&self.diff_snapshot(DiffMode::Worktree, DiffOptions::default()));
            if !diff_paths.contains(rel_str.as_str()) {
                return make_result(
                    call,
                    ToolStatus::Denied,
                    json!({ "error": "refusing to read a clean file because diff_only=true", "path": rel_str }),
                    ToolCostHint::default(),
                    None,
                );
            }
        }
        if is_secret_path(&rel) {
            return make_result(
                call,
                ToolStatus::Denied,
                json!({ "error": "refusing to read a likely secret file" }),
                ToolCostHint::default(),
                None,
            );
        }
        if diff_mode {
            let ctx = ReadSliceDiffCtx {
                call,
                args: &args,
                path: &path,
                rel: rel_str.as_str(),
                graph_available: graph.is_some(),
                graph_status,
                confidence,
                provenance,
                span,
            };
            return self.execute_read_slice_diff_blocking(&ctx);
        }
        let path = match canonicalize_workspace_root(&path) {
            Ok(path) => path,
            Err(err) => {
                return tool_error(
                    call,
                    format!("path does not exist or is inaccessible: {err}"),
                );
            }
        };

        let total_bytes = match file_len(&path) {
            Ok(len) => len,
            Err(err) => return tool_error(call, err),
        };
        let prefix_bytes = read_prefix(&path, POLICY_PREFIX_BYTES).ok();
        let ignored_reason = self
            .policy_exclusion_for_file(&path, &rel, prefix_bytes.as_deref())
            .map(ExclusionReason::as_str);
        let (offset, limit, resolved_span) =
            match read_slice_byte_window(&path, total_bytes, &args, span) {
                Ok(window) => window,
                Err(err) => return tool_error(call, err),
            };
        let bytes = match read_range(&path, offset as u64, limit) {
            Ok(bytes) => bytes,
            Err(err) => return tool_error(call, err),
        };
        let end = offset.saturating_add(bytes.len());
        let content = String::from_utf8_lossy(&bytes).to_string();
        let content_sha256 = match sha256_file(&path) {
            Ok(hash) => hash,
            Err(err) => return tool_error(call, err),
        };
        let truncated = end < total_bytes as usize
            && args
                .end_byte
                .or_else(|| resolved_span.map(|span| span.end_byte as usize))
                .map(|requested_end| end < requested_end)
                .unwrap_or(end < total_bytes as usize);
        let cost = ToolCostHint {
            bytes_read: bytes.len() as u64,
            output_bytes: content.len() as u64,
            truncated,
            ..ToolCostHint::default()
        };
        let mut packet = evidence_packet(
            "read_slice returned a bounded exact file slice",
            vec![span_for_path_json(&rel_str, resolved_span)],
            confidence,
            Freshness::Fresh,
            provenance,
            cost.clone(),
            json!({
                "tool": "read_file",
                "arguments": {
                    "path": &rel_str,
                    "offset": end,
                    "limit": DEFAULT_READ_LIMIT
                },
                "reason": "continue reading after this slice if more context is needed"
            }),
        );
        if let Some(object) = packet.as_object_mut() {
            object.insert("path".to_string(), json!(&rel_str));
            object.insert("offset".to_string(), json!(offset));
            object.insert("bytes_returned".to_string(), json!(bytes.len()));
        }
        let mut payload = serde_json::Map::new();
        payload.insert("tool".to_string(), json!("read_slice"));
        payload.insert("graph_available".to_string(), json!(graph.is_some()));
        payload.insert("graph_status".to_string(), json!(graph_status));
        payload.insert("path".to_string(), json!(&rel_str));
        payload.insert("offset".to_string(), json!(offset));
        payload.insert("bytes_returned".to_string(), json!(bytes.len()));
        payload.insert("total_bytes".to_string(), json!(total_bytes));
        payload.insert("sha256".to_string(), json!(&content_sha256));
        payload.insert("truncated".to_string(), json!(truncated));
        if let Some(reason) = ignored_reason {
            payload.insert("ignored".to_string(), json!(true));
            payload.insert("ignored_reason".to_string(), json!(reason));
        }
        payload.insert("packets".to_string(), json!([packet]));
        payload.insert("content".to_string(), json!(content));
        make_result(
            call,
            ToolStatus::Success,
            Value::Object(payload),
            cost,
            Some(content_sha256),
        )
    }

    fn execute_read_slice_diff_blocking(&self, ctx: &ReadSliceDiffCtx<'_>) -> ToolResult {
        let baseline_requested = ctx.args.diff_baseline.unwrap_or_default();
        if baseline_requested == DiffReadBaseline::LastReceipt {
            match self.read_slice_last_receipt_diff(ctx) {
                LastReceiptDiffOutcome::Result(result) => return *result,
                LastReceiptDiffOutcome::Fallback(reason) => {
                    return self.read_slice_git_diff(
                        ctx,
                        DiffReadBaseline::Worktree,
                        Some(json!({
                            "requested": diff_read_baseline_str(baseline_requested),
                            "used": diff_read_baseline_str(DiffReadBaseline::Worktree),
                            "reason": reason,
                        })),
                    );
                }
            }
        }
        self.read_slice_git_diff(ctx, baseline_requested, None)
    }

    fn read_slice_git_diff(
        &self,
        ctx: &ReadSliceDiffCtx<'_>,
        baseline: DiffReadBaseline,
        baseline_fallback: Option<Value>,
    ) -> ToolResult {
        let ReadSliceDiffCtx {
            call,
            args,
            path,
            rel,
            graph_available,
            graph_status,
            confidence,
            provenance,
            ..
        } = ctx;
        let rel = *rel;
        let snapshot_mode = match baseline {
            DiffReadBaseline::Worktree | DiffReadBaseline::LastReceipt => DiffMode::Worktree,
            DiffReadBaseline::BranchBase => DiffMode::BranchBase,
            DiffReadBaseline::Index => DiffMode::Index,
        };
        let max_ranges = args.max_ranges.unwrap_or(20).clamp(1, 100);
        // Cap diff reads at the same per-file budget the crawler uses to skip
        // oversized files. A multi-hundred-MB modified file would otherwise be
        // fully slurped before any per-range cap applies; the slice mode
        // already pages through `read_slice_byte_window`, but the diff path
        // needs the whole current file to attribute hunks to byte ranges.
        if let Ok(size) = file_len(path) {
            let limit = self.crawl_options.max_file_bytes;
            if limit > 0 && size > limit {
                let mut payload = serde_json::Map::new();
                payload.insert("tool".to_string(), json!("read_slice"));
                payload.insert("read_mode".to_string(), json!("diff"));
                payload.insert("status".to_string(), json!("file_too_large"));
                payload.insert("graph_available".to_string(), json!(*graph_available));
                payload.insert("graph_status".to_string(), json!(*graph_status));
                payload.insert(
                    "baseline_requested".to_string(),
                    json!(diff_read_baseline_str(
                        args.diff_baseline.unwrap_or_default()
                    )),
                );
                payload.insert(
                    "baseline_used".to_string(),
                    json!(diff_read_baseline_str(baseline)),
                );
                if let Some(fallback) = baseline_fallback {
                    payload.insert("baseline_fallback".to_string(), fallback);
                }
                payload.insert("path".to_string(), json!(rel));
                payload.insert("total_bytes".to_string(), json!(size));
                payload.insert("max_file_bytes".to_string(), json!(limit));
                payload.insert("ranges".to_string(), json!([]));
                payload.insert("packets".to_string(), json!([]));
                payload.insert("truncated".to_string(), json!(true));
                return make_result(
                    call,
                    ToolStatus::Denied,
                    Value::Object(payload),
                    ToolCostHint {
                        truncated: true,
                        ..ToolCostHint::default()
                    },
                    None,
                );
            }
        }
        let snapshot = self.diff_snapshot(
            snapshot_mode,
            DiffOptions {
                include_patch: true,
                max_patch_bytes: 5_000_000,
            },
        );
        let file = snapshot.files.iter().find(|file| file.path == rel);
        let content_sha256 = path.exists().then(|| sha256_file(path).ok()).flatten();
        let current_text = path
            .exists()
            .then(|| fs::read(path).ok())
            .flatten()
            .map(|bytes| String::from_utf8_lossy(&bytes).to_string());
        let mut cost = ToolCostHint::default();
        let mut truncated = snapshot.truncated;
        let mut ranges = Vec::new();
        let mut packets = Vec::new();

        if let Some(file) = file {
            if file.binary {
                packets.push(read_diff_packet(
                    rel,
                    None,
                    "read_slice diff found a changed binary file; source bytes were omitted",
                    *confidence,
                    provenance,
                    ToolCostHint::default(),
                    DiffNextActionKind::ReadSlice,
                ));
                ranges.push(json!({
                    "status": diff_status_str(file.status),
                    "binary": true,
                    "content_omitted": "binary_file",
                }));
            } else if file.status == DiffFileStatus::Deleted {
                packets.push(read_diff_packet(
                    rel,
                    None,
                    "read_slice diff found a deleted file; current source bytes are unavailable",
                    *confidence,
                    provenance,
                    ToolCostHint::default(),
                    DiffNextActionKind::ReadSlice,
                ));
                ranges.push(json!({
                    "status": "deleted",
                    "content_omitted": "deleted_file",
                }));
            } else if let Some(text) = current_text.as_deref() {
                let changed_ranges = file
                    .patch
                    .as_deref()
                    .map(|patch| changed_byte_ranges_from_patch(patch, text))
                    .filter(|ranges| !ranges.is_empty())
                    .unwrap_or_else(|| {
                        if file.status == DiffFileStatus::Added {
                            vec![ChangedByteRange::new(
                                0,
                                text.len(),
                                1,
                                text.lines().count().max(1) as u32,
                                "added",
                            )]
                        } else {
                            diff_hunks_to_byte_ranges(&file.hunks, text)
                        }
                    });
                for range in changed_ranges.into_iter().take(max_ranges) {
                    let bytes = text.as_bytes();
                    let capped_end = range.end.min(range.start.saturating_add(MAX_READ_LIMIT));
                    let content =
                        String::from_utf8_lossy(&bytes[range.start..capped_end]).to_string();
                    let range_truncated = capped_end < range.end;
                    truncated |= range_truncated;
                    let range_cost = ToolCostHint {
                        bytes_read: content.len() as u64,
                        output_bytes: content.len() as u64,
                        truncated: range_truncated,
                        ..ToolCostHint::default()
                    };
                    cost.bytes_read += range_cost.bytes_read;
                    cost.truncated |= range_truncated;
                    let span = SourceSpan::new(
                        range.start.min(u32::MAX as usize) as u32,
                        range.end.min(u32::MAX as usize) as u32,
                        squeezy_core::SourcePoint::new(range.start_line.saturating_sub(1), 0),
                        squeezy_core::SourcePoint::new(range.end_line.saturating_sub(1), 0),
                    );
                    packets.push(read_diff_packet(
                        rel,
                        Some(span),
                        "read_slice diff returned changed source bytes",
                        *confidence,
                        provenance,
                        range_cost,
                        // Range carries `content` inline, so steer the model to
                        // graph-backed context for the enclosing symbol rather
                        // than re-fetching the same bytes via slice mode.
                        DiffNextActionKind::SymbolContextOrSlice {
                            rust_graph: *graph_available && *graph_status == "rust",
                        },
                    ));
                    ranges.push(json!({
                        "status": range.status,
                        "start_byte": range.start,
                        "end_byte": range.end,
                        "start_line": range.start_line,
                        "end_line": range.end_line,
                        "bytes_returned": content.len(),
                        "truncated": range_truncated,
                        "content": content,
                    }));
                }
                truncated |= file.patch_truncated
                    || (ranges.len() >= max_ranges && file.hunks.len() > max_ranges);
            }
        }

        if ranges.is_empty() {
            packets.push(read_diff_packet(
                rel,
                None,
                "read_slice diff found no changed source ranges for this path",
                *confidence,
                provenance,
                ToolCostHint::default(),
                DiffNextActionKind::ReadSlice,
            ));
        }

        cost.matches_returned = ranges.len() as u64;
        cost.truncated |= truncated;
        let mut payload = serde_json::Map::new();
        payload.insert("tool".to_string(), json!("read_slice"));
        payload.insert("read_mode".to_string(), json!("diff"));
        payload.insert("graph_available".to_string(), json!(*graph_available));
        payload.insert("graph_status".to_string(), json!(*graph_status));
        payload.insert(
            "baseline_requested".to_string(),
            json!(diff_read_baseline_str(
                args.diff_baseline.unwrap_or_default()
            )),
        );
        payload.insert(
            "baseline_used".to_string(),
            json!(diff_read_baseline_str(baseline)),
        );
        if let Some(fallback) = baseline_fallback {
            payload.insert("baseline_fallback".to_string(), fallback);
        }
        payload.insert("path".to_string(), json!(rel));
        payload.insert("sha256".to_string(), json!(&content_sha256));
        // `path_in_diff` is the literal "git reports a diff for this path"
        // signal: it stays true even when no source ranges survive (e.g.
        // binary/deleted files) so callers can distinguish "clean file" from
        // "changed file with no readable content".
        let path_in_diff = file.is_some();
        payload.insert("path_in_diff".to_string(), json!(path_in_diff));
        // Back-compat alias: pre-fix consumers read `unchanged` to mean "git
        // reports no diff for this path". Keep the same wire shape but only
        // claim "unchanged" when both git is clean *and* we produced no
        // packets, so binary/deleted entries no longer masquerade as clean.
        payload.insert(
            "unchanged".to_string(),
            json!(!path_in_diff && ranges.is_empty()),
        );
        payload.insert("ranges".to_string(), json!(ranges));
        payload.insert("packets".to_string(), json!(packets));
        payload.insert("truncated".to_string(), json!(cost.truncated));
        payload.insert("vcs".to_string(), json!(snapshot.vcs));
        payload.insert("errors".to_string(), json!(snapshot.errors));
        make_result(
            call,
            ToolStatus::Success,
            Value::Object(payload),
            cost,
            content_sha256,
        )
    }

    fn read_slice_last_receipt_diff(&self, ctx: &ReadSliceDiffCtx<'_>) -> LastReceiptDiffOutcome {
        let ReadSliceDiffCtx {
            call,
            args,
            path,
            rel,
            graph_available,
            graph_status,
            confidence,
            provenance,
            span,
        } = ctx;
        let rel = *rel;
        let Some(store) = self.state_store.as_deref() else {
            return LastReceiptDiffOutcome::Fallback("last_receipt_store_unavailable");
        };
        let snapshots = match store.read_snapshots_for_path(rel) {
            Ok(snapshots) => snapshots,
            Err(_) => return LastReceiptDiffOutcome::Fallback("last_receipt_store_error"),
        };
        if snapshots.is_empty() {
            return LastReceiptDiffOutcome::Fallback("last_receipt_snapshot_missing");
        }
        let content_sha256 = match sha256_file(path) {
            Ok(hash) => hash,
            Err(_) => {
                return LastReceiptDiffOutcome::Fallback("last_receipt_current_file_unavailable");
            }
        };
        let total_bytes = match file_len(path) {
            Ok(len) => len,
            Err(_) => {
                return LastReceiptDiffOutcome::Fallback("last_receipt_current_file_unavailable");
            }
        };
        let (offset, limit, _) = match read_slice_byte_window(path, total_bytes, args, *span) {
            Ok(window) => window,
            Err(_) => return LastReceiptDiffOutcome::Fallback("last_receipt_window_unavailable"),
        };
        let end = offset.saturating_add(limit).min(total_bytes as usize);
        // Pick the most recent snapshot whose stored window exactly matches the
        // requested `[offset, end)`. Storage is keyed by
        // `(path, start_byte, end_byte)`, so distinct windows of the same file
        // do not overwrite each other and the requested window is found even
        // if a subsequent unrelated read landed for the same path.
        let snapshot = snapshots
            .iter()
            .filter(|snapshot| {
                snapshot.start_byte == offset as u64 && snapshot.end_byte == end as u64
            })
            .max_by_key(|snapshot| snapshot.created_unix_millis);
        let snapshot = match snapshot {
            Some(snapshot) => snapshot.clone(),
            None => return LastReceiptDiffOutcome::Fallback("last_receipt_window_mismatch"),
        };
        if snapshot.content_sha256.as_deref() == Some(content_sha256.as_str()) {
            // `cost_hint.truncated` semantically means "we omitted bytes the
            // caller would have got". The unchanged stub omits bytes by design
            // because they were unchanged, so report it as a dedup save rather
            // than a truncation — the latter biases the budget broker against
            // the receipt-stub path.
            let cost = ToolCostHint::default();
            let packet = read_diff_packet(
                rel,
                None,
                "read_slice diff found no changes since the last receipt",
                *confidence,
                provenance,
                cost.clone(),
                DiffNextActionKind::ReadSlice,
            );
            return LastReceiptDiffOutcome::Result(Box::new(make_result(
                call,
                ToolStatus::Success,
                json!({
                    "tool": "read_slice",
                    "read_mode": "diff",
                    "graph_available": *graph_available,
                    "graph_status": *graph_status,
                    "baseline_requested": "last_receipt",
                    "baseline_used": "last_receipt",
                    "path": rel,
                    "sha256": &content_sha256,
                    "unchanged": true,
                    "receipt_stub": true,
                    "dedup": true,
                    "same_as_call_id": snapshot.call_id,
                    "same_as_tool_name": snapshot.tool_name,
                    "original_output_sha256": snapshot.stable_output_sha256,
                    "original_content_sha256": snapshot.content_sha256,
                    "original_model_output_bytes": snapshot.model_output_bytes,
                    "ranges": [],
                    "packets": [packet],
                    "truncated": false,
                }),
                cost,
                Some(content_sha256.clone()),
            )));
        }

        let bytes = match read_range(path, offset as u64, limit) {
            Ok(bytes) => bytes,
            Err(_) => {
                return LastReceiptDiffOutcome::Fallback("last_receipt_current_file_unavailable");
            }
        };
        let current = String::from_utf8_lossy(&bytes).to_string();
        // Line numbers must be reported against the full file, not against the
        // window. `current` covers `[offset, offset+limit)` only, so count the
        // newlines that precede `offset` once and apply that offset to each
        // window-local line number.
        let line_offset_before_window = match window_line_offset(path, offset) {
            Ok(value) => value,
            Err(_) => {
                return LastReceiptDiffOutcome::Fallback("last_receipt_current_file_unavailable");
            }
        };
        let local_ranges = byte_diff_ranges(snapshot.content.as_bytes(), current.as_bytes());
        let mut cost = ToolCostHint::default();
        let mut ranges = Vec::new();
        let mut packets = Vec::new();
        for range in local_ranges
            .into_iter()
            .take(args.max_ranges.unwrap_or(20).clamp(1, 100))
        {
            let start = offset.saturating_add(range.start);
            let end_bytes = offset.saturating_add(range.end);
            let capped_end = range.end.min(range.start.saturating_add(MAX_READ_LIMIT));
            let content =
                String::from_utf8_lossy(&current.as_bytes()[range.start..capped_end]).to_string();
            let range_truncated = capped_end < range.end;
            let start_line = line_number_for_byte(&current, range.start)
                .saturating_add(line_offset_before_window);
            let end_line =
                line_number_for_byte(&current, range.end.saturating_sub(1).max(range.start))
                    .saturating_add(line_offset_before_window);
            let span = SourceSpan::new(
                start.min(u32::MAX as usize) as u32,
                end_bytes.min(u32::MAX as usize) as u32,
                squeezy_core::SourcePoint::new(start_line.saturating_sub(1), 0),
                squeezy_core::SourcePoint::new(end_line.saturating_sub(1), 0),
            );
            let range_cost = ToolCostHint {
                bytes_read: content.len() as u64,
                output_bytes: content.len() as u64,
                truncated: range_truncated,
                ..ToolCostHint::default()
            };
            cost.bytes_read += range_cost.bytes_read;
            cost.truncated |= range_truncated;
            packets.push(read_diff_packet(
                rel,
                Some(span),
                "read_slice diff returned source bytes changed since the last receipt",
                *confidence,
                provenance,
                range_cost,
                DiffNextActionKind::SymbolContextOrSlice {
                    rust_graph: *graph_available && *graph_status == "rust",
                },
            ));
            ranges.push(json!({
                "status": "modified",
                "start_byte": start,
                "end_byte": end_bytes,
                "start_line": start_line,
                "end_line": end_line,
                "bytes_returned": content.len(),
                "truncated": range_truncated,
                "content": content,
            }));
        }
        if ranges.is_empty() {
            packets.push(read_diff_packet(
                rel,
                None,
                "read_slice diff found no changes in the last receipt window",
                *confidence,
                provenance,
                ToolCostHint::default(),
                DiffNextActionKind::ReadSlice,
            ));
        }
        cost.matches_returned = ranges.len() as u64;
        let result = make_result(
            call,
            ToolStatus::Success,
            json!({
                "tool": "read_slice",
                "read_mode": "diff",
                "graph_available": *graph_available,
                "graph_status": *graph_status,
                "baseline_requested": "last_receipt",
                "baseline_used": "last_receipt",
                "path": rel,
                "sha256": &content_sha256,
                "unchanged": false,
                "ranges": ranges,
                "packets": packets,
                "truncated": cost.truncated,
            }),
            cost,
            Some(content_sha256.clone()),
        );
        LastReceiptDiffOutcome::Result(Box::new(result))
    }

    pub(crate) fn graph_context_for_snapshot(
        &self,
        snapshot: &DiffSnapshot,
        max_symbols_per_file: usize,
        max_references: usize,
    ) -> Value {
        self.wait_for_graph_ready(GRAPH_READY_WAIT);
        let mut graph = match self.graph.lock() {
            Ok(graph) => graph,
            Err(_) => return json!({"available": false, "error": "semantic graph lock poisoned"}),
        };
        let Some(manager) = graph.as_mut() else {
            return json!({"available": false, "reason": "semantic graph is unavailable for this workspace"});
        };
        let refresh = manager.refresh_before_query().ok();
        annotate_graph(manager, snapshot);
        let graph = manager.graph();
        let dirty = graph.dirty_symbols();
        let mut by_file: BTreeMap<String, Vec<GraphSymbol>> = BTreeMap::new();
        for symbol in dirty {
            by_file
                .entry(symbol.file_id.0.clone())
                .or_default()
                .push(symbol);
        }
        let files = by_file
            .into_iter()
            .map(|(path, symbols)| {
                let total = symbols.len();
                let symbols = symbols
                    .iter()
                    .take(max_symbols_per_file)
                    .map(|symbol| symbol_context_json(graph, symbol, max_references))
                    .collect::<Vec<_>>();
                json!({
                    "path": path,
                    "symbols": symbols,
                    "truncated": total > max_symbols_per_file,
                })
            })
            .collect::<Vec<_>>();
        let mut payload = serde_json::Map::new();
        payload.insert("available".to_string(), json!(true));
        if let Some(report) = refresh {
            let mut refresh_obj = serde_json::Map::new();
            refresh_obj.insert("refreshed".to_string(), json!(report.refreshed));
            refresh_obj.insert(
                "changed_files".to_string(),
                json!(
                    report
                        .changed_files
                        .iter()
                        .map(|id| id.0.clone())
                        .collect::<Vec<_>>()
                ),
            );
            refresh_obj.insert(
                "removed_files".to_string(),
                json!(
                    report
                        .removed_files
                        .iter()
                        .map(|id| id.0.clone())
                        .collect::<Vec<_>>()
                ),
            );
            refresh_obj.insert("reparsed_files".to_string(), json!(report.reparsed_files));
            refresh_obj.insert("excluded_files".to_string(), json!(report.excluded_files));
            refresh_obj.insert("excluded_dirs".to_string(), json!(report.excluded_dirs));
            refresh_obj.insert("excluded_bytes".to_string(), json!(report.excluded_bytes));
            if let Some(coverage) = coverage_json(&report.coverage) {
                refresh_obj.insert("coverage".to_string(), coverage);
            }
            refresh_obj.insert(
                "budget_exhausted".to_string(),
                json!(report.budget_exhausted),
            );
            payload.insert("refresh".to_string(), Value::Object(refresh_obj));
        }
        if let Some(coverage) = coverage_json(&manager.build_report().coverage) {
            payload.insert("coverage".to_string(), coverage);
        }
        payload.insert("files".to_string(), json!(files));
        Value::Object(payload)
    }
}
