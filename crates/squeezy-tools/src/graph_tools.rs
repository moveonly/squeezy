use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs,
    path::Path,
    time::Duration,
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
    SourceCache,
};
use squeezy_vcs::{
    DiffFileStatus, DiffHunk, DiffMode, DiffOptions, DiffSnapshot, canonicalize_workspace_root,
};
use squeezy_workspace::ExclusionReason;

use crate::{
    DEFAULT_GRAPH_MAX_DEPTH, DEFAULT_GRAPH_MAX_RESULTS, DEFAULT_READ_LIMIT,
    GRAPH_READ_SLICE_MAX_LINE_SCAN_BYTES, MAX_GRAPH_MAX_DEPTH, MAX_GRAPH_MAX_RESULTS,
    MAX_READ_LIMIT, POLICY_PREFIX_BYTES, ToolCall, ToolCostHint, ToolRegistry, ToolResult,
    ToolStatus, diff_mode_str, diff_path_set, diff_status_str, file_len, graph_ready_wait,
    is_secret_path, make_result, read_prefix, read_range, sha256_file, tool_arg_error, tool_error,
    workspace_path,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SymbolContextArgs {
    pub(crate) query: String,
    /// Optional symbol id minted by a sibling tool (definition_search, flow,
    /// etc.). When live, it resolves the target directly; a stale id falls
    /// through to the `query` search.
    symbol_id: Option<String>,
    path: Option<String>,
    diff_only: Option<bool>,
    mode: Option<DiffMode>,
    /// Restrict resolved symbols to a single language. See `language_matches`.
    language: Option<String>,
    /// Filter result packets to symbols in test code only / excluding tests.
    exclude_tests: Option<bool>,
    tests_only: Option<bool>,
    /// Pipe-separated confidence allow-set (e.g. `exact_syntax|import_resolved`).
    /// See [`ConfidenceScope`].
    confidence: Option<String>,
    /// Drop out-of-workspace (`External`) targets.
    exclude_external: Option<bool>,
    /// Keep ONLY out-of-workspace (`External`) targets (wins over
    /// `exclude_external`).
    external_only: Option<bool>,
    max_references: Option<usize>,
    max_results: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoMapArgs {
    /// Restrict the map to roots whose declaring file is under this path scope.
    path: Option<String>,
    /// Restrict the map to roots in a single language.
    language: Option<String>,
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
    transitive: Option<bool>,
    /// Dead-code mode: when `true`, retain only scanned declarations that have
    /// zero inbound `Calls`/`References`/`TestOf` edges (no resolved use). The
    /// usual query/kind/path/language/visibility/attribute filters still scope
    /// the candidate set first. Public/exported candidates are flagged rather
    /// than dropped, and candidates whose only edges resolve at a low
    /// confidence are caveated, never silently reported as dead.
    unused: Option<bool>,
    /// In `unused` mode, the inbound-edge count above which a declaration is
    /// considered "used" and excluded. Defaults to 0 (strictly zero inbound
    /// edges). Lets a caller surface near-dead declarations (e.g. used once).
    max_callers: Option<usize>,
    /// Drop test-code declarations from the result (see `path_is_test`).
    exclude_tests: Option<bool>,
    /// Keep ONLY test-code declarations. Wins over `exclude_tests` when both set.
    tests_only: Option<bool>,
    /// Pipe-separated confidence allow-set (see [`ConfidenceScope`]).
    confidence: Option<String>,
    /// Drop out-of-workspace (`External`) declarations.
    exclude_external: Option<bool>,
    /// Keep ONLY out-of-workspace (`External`) declarations.
    external_only: Option<bool>,
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
    /// Pipe-separated confidence allow-set (see [`ConfidenceScope`]).
    confidence: Option<String>,
    /// Drop out-of-workspace (`External`) candidates.
    exclude_external: Option<bool>,
    /// Keep ONLY out-of-workspace (`External`) candidates.
    external_only: Option<bool>,
    max_results: Option<usize>,
    /// Skip the first N candidates after sorting, before `max_results`. Lets a
    /// caller page through a large candidate set.
    offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceSearchArgs {
    query: Option<String>,
    text: Option<String>,
    symbol_id: Option<String>,
    path: Option<String>,
    /// Restrict reference hits to a single language (matched against the
    /// declaring file of each hit). See `language_matches`.
    language: Option<String>,
    /// Restrict to a reference kind (e.g. `call`, `import`); see
    /// `reference_kind_matches`.
    reference_kind: Option<String>,
    exclude_tests: Option<bool>,
    tests_only: Option<bool>,
    /// Pipe-separated confidence allow-set over the reference hits (see
    /// [`ConfidenceScope`]).
    confidence: Option<String>,
    /// Drop out-of-workspace (`External`) reference hits.
    exclude_external: Option<bool>,
    /// Keep ONLY out-of-workspace (`External`) reference hits.
    external_only: Option<bool>,
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
    /// Restrict the resolved root (when resolved by query) to a single language.
    language: Option<String>,
    /// Restrict result packets to a single edge kind
    /// (`Calls`|`References`|`Imports`|`Reexports`); see `edge_kind_matches`.
    edge_kind: Option<String>,
    /// Filter the RESULT packets (not just the root) to this path scope.
    result_path: Option<String>,
    exclude_tests: Option<bool>,
    tests_only: Option<bool>,
    /// Pipe-separated confidence allow-set over the result packets (see
    /// [`ConfidenceScope`]).
    confidence: Option<String>,
    /// Drop out-of-workspace (`External`) result packets.
    exclude_external: Option<bool>,
    /// Keep ONLY out-of-workspace (`External`) result packets.
    external_only: Option<bool>,
    target_symbol_id: Option<String>,
    target_query: Option<String>,
    max_depth: Option<usize>,
    max_results: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HierarchyArgs {
    symbol_id: Option<String>,
    query: Option<String>,
    kind: Option<String>,
    path: Option<String>,
    /// Restrict the resolved root (when resolved by query) to a single language.
    language: Option<String>,
    /// Filter the RESULT nodes (not just the root) to this path scope.
    result_path: Option<String>,
    exclude_tests: Option<bool>,
    tests_only: Option<bool>,
    max_depth: Option<usize>,
    max_results: Option<usize>,
    offset: Option<usize>,
}

/// Arguments for the `impact` graph tool. Accepts a symbol, a file path, or
/// a set of changed file paths to drive the affected-set computation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImpactArgs {
    /// Symbol whose declaring file is treated as the changed root.
    symbol_id: Option<String>,
    /// Direct query to resolve to a symbol (used when `symbol_id` is absent).
    query: Option<String>,
    /// File path treated as the changed root.
    path: Option<String>,
    /// Additional file paths that are also part of the changed set.
    #[serde(default)]
    extra_paths: Vec<String>,
    /// When `true`, return ONLY the files that directly import each changed
    /// file (one BFS hop, no transitive fan-out). Skips the
    /// `affected_symbols`/`affected_tests` computation entirely. Defaults to
    /// `false` (the full transitive impact).
    direct_only: Option<bool>,
    /// Restrict the affected-symbol packets to a single language.
    language: Option<String>,
    /// Drop / keep-only test-code symbols among the affected set.
    exclude_tests: Option<bool>,
    tests_only: Option<bool>,
    /// Pipe-separated confidence allow-set over the affected symbols (see
    /// [`ConfidenceScope`]).
    confidence: Option<String>,
    /// Drop out-of-workspace (`External`) affected symbols.
    exclude_external: Option<bool>,
    /// Keep ONLY out-of-workspace (`External`) affected symbols.
    external_only: Option<bool>,
    /// Maximum number of affected symbols to return (default 50).
    max_results: Option<usize>,
    /// Skip the first N affected symbols after sorting, before `max_results`.
    offset: Option<usize>,
}

/// Arguments for the `symbol_at` graph tool: resolve a source position
/// (line, or byte offset) inside a file to the smallest enclosing symbol.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolAtArgs {
    /// File path to resolve the position inside.
    path: String,
    /// 1-based line number. Used when `byte` is absent.
    line: Option<u32>,
    /// Column (currently advisory; the line span already pins the symbol).
    /// Accepted so editor cursors can pass a full position without an error.
    #[allow(dead_code)]
    column: Option<u32>,
    /// Byte offset into the file. When set, takes precedence over `line`.
    byte: Option<u32>,
}

/// Arguments for the `inheritance_hierarchy` graph tool.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InheritanceHierarchyArgs {
    /// Symbol whose ancestors or descendants are requested.
    symbol_id: Option<String>,
    /// Text query to resolve to a class/struct/interface symbol.
    query: Option<String>,
    /// When `false` (default), return all transitive supertypes of the root
    /// via a BFS over `UsesTrait`/`Extends`/`Implements` edges (ancestors).
    /// When `true`, return the direct subtypes of the root (symbols that carry
    /// an edge pointing *to* the root). Combine with `transitive=true` to return
    /// the full subtype closure instead of just the first generation.
    subtypes: Option<bool>,
    /// When `true` (with `subtypes=true`), return the transitive subtype closure
    /// via a bounded BFS over the inheritance edges instead of only the direct
    /// subtypes. Ignored when `subtypes` is unset (ancestors are already a
    /// transitive walk) or when in member mode. See `max_depth`.
    transitive: Option<bool>,
    /// When `true` and the resolved root is a member (Method/Field), return the
    /// overrides/implementations of that member across transitive subtypes
    /// instead of the type-to-type ancestor/subtype walk. Ignored (falls back to
    /// the type walk) when the root is not a member.
    member: Option<bool>,
    /// Restrict the resolved root (when resolved by query) and the related
    /// symbols to a single language. See `language_matches`.
    language: Option<String>,
    /// Transitive subtype walk depth when `subtypes=true` and `transitive=true`.
    /// Clamped to the graph's traversal bounds; defaults to the standard
    /// depth-bounded-walk default. Ignored unless the transitive subtype closure
    /// is requested.
    max_depth: Option<usize>,
    /// Pipe-separated confidence allow-set over the related symbols (see
    /// [`ConfidenceScope`]).
    confidence: Option<String>,
    /// Drop out-of-workspace (`External`) related symbols.
    exclude_external: Option<bool>,
    /// Keep ONLY out-of-workspace (`External`) related symbols.
    external_only: Option<bool>,
    /// Maximum results (default 50).
    max_results: Option<usize>,
    /// Skip the first N related symbols after sorting, before `max_results`.
    offset: Option<usize>,
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
    /// Policy-exclusion reason for this file, mirrored from slice mode so the
    /// diff payload can advertise `ignored`/`ignored_reason` too. Slice mode
    /// surfaces this; without it a model can't tell a policy-excluded file
    /// apart from a clean one when reading in diff mode.
    ignored_reason: Option<&'static str>,
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
    let mut packet = evidence_packet(
        claim,
        vec![span_for_path_json(path, span)],
        confidence,
        Freshness::Fresh,
        provenance.to_vec(),
        cost_hint,
        read_diff_next_action(path, next_action_kind),
    );
    // Bare packet (no symbol/reference/edge body), so keep a minimal
    // spans + confidence here — the diff `ranges` sibling carries content but
    // is not part of the packet, and this is the only path+span the model has.
    if let Some(object) = packet.as_object_mut() {
        object.insert(
            "spans".to_string(),
            json!(vec![span_for_path_json(path, span)]),
        );
        object.insert("confidence".to_string(), json!(confidence.id()));
    }
    packet
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
        // Before the first `@@`, `new_line == 0`, which already skips the
        // unified-diff file headers (`+++ b/file`, `--- a/file`). Inside a
        // hunk every body line carries a single-char prefix (`+`/`-`/` `), so
        // classifying by that prefix alone correctly handles content lines
        // whose text begins with `++`/`--` (e.g. `+++…` for an added `++i;`
        // or a `+++` frontmatter delimiter, `----` for a deleted `---` rule).
        if new_line == 0 {
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
pub(crate) fn window_line_offset(
    path: &Path,
    offset: usize,
) -> std::result::Result<u32, std::io::Error> {
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

/// Translate a 1-based, inclusive `[start_line, end_line]` request into the
/// byte `(offset, limit)` window that `read_file` (which is byte-addressed)
/// expects. `start_line` defaults to 1 and `end_line` defaults to the end of
/// file. Streams the file in 8 KiB chunks so large files do not have to be
/// slurped into memory. `end_line` is clamped to be at least `start_line`.
///
/// Returns `(byte_offset_of_start_line, byte_len_through_end_line)`. When
/// `end_line` is omitted the limit covers the rest of the file from
/// `start_line`. A `start_line` past EOF yields an offset at EOF with a zero
/// limit (the handler then returns an empty—but successful—slice rather than
/// erroring).
pub(crate) fn byte_window_for_line_range(
    path: &Path,
    start_line: Option<u32>,
    end_line: Option<u32>,
) -> std::result::Result<(usize, usize), std::io::Error> {
    use std::io::{BufReader, Read};
    let start_line = start_line.unwrap_or(1).max(1);
    let end_line = end_line.map(|end| end.max(start_line));

    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut buf = [0u8; 8192];

    // 1-based line we are currently inside; advances on every '\n'.
    let mut current_line: u32 = 1;
    let mut byte_pos: usize = 0;
    let mut start_offset: Option<usize> = None;
    // Byte position just past the last newline that closes `end_line`.
    let mut end_offset: Option<usize> = None;

    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        for &byte in &buf[..read] {
            if start_offset.is_none() && current_line == start_line {
                start_offset = Some(byte_pos);
            }
            byte_pos += 1;
            if byte == b'\n' {
                if let Some(end_line) = end_line
                    && current_line == end_line
                    && end_offset.is_none()
                {
                    end_offset = Some(byte_pos);
                }
                current_line = current_line.saturating_add(1);
            }
        }
        if start_offset.is_some() && end_offset.is_some() {
            break;
        }
    }

    // `start_line` may begin exactly at EOF (file ends with a newline and the
    // request points one past the last line) — settle on the EOF position.
    let start = start_offset.unwrap_or(byte_pos);
    // No closing newline for `end_line` (last line unterminated, or end_line
    // beyond EOF): read through the end of file.
    let end = end_offset.unwrap_or(byte_pos).max(start);
    Ok((start, end - start))
}

fn symbol_matches_path_filter(symbol: &GraphSymbol, filter: Option<&str>) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    path_matches_filter(symbol.file_id.0.as_str(), filter)
}

/// Normalise Windows-style backslash separators in a user-supplied filter to
/// forward slashes. Graph paths are always slash-normalised by workspace
/// discovery, so this lets users paste paths from Explorer, PowerShell,
/// `cargo` output, or MSBuild output without learning the internal convention.
///
/// Returns the input unchanged (zero allocation) when it contains no `\`.
/// Match `path` against a model-supplied `filter`.
///
/// Filters that look like a directory path (contain `/` or `\`) match by
/// strict prefix with a directory boundary — `gson/src/main/java` matches
/// files under that tree but not siblings like `gson/src/test/java/...`.
/// Windows-pasted filters such as `gson\src\main\java` are normalised to
/// forward slashes first so they match the slash-normalised graph paths.
///
/// Single-token filters (no directory separator) keep the loose
/// trailing-segment + fuzzy fallback so casual "find a crate" queries
/// still resolve. The fuzzy path was the source of cross-tree noise only
/// when the model already wrote a real prefix; gating on separators removes
/// that noise without regressing the bareword UX.
fn path_matches_filter(path: &str, filter: &str) -> bool {
    // Normalize backslashes and leading `./` once before all comparisons so
    // Windows-style input like `.\src\app.cs` resolves against `src/app.cs`.
    let filter_owned = normalize_path_filter(filter);
    let filter = filter_owned.as_ref();
    if filter.contains('/') {
        let filter = filter.trim_end_matches('/');
        if filter.is_empty() {
            return true;
        }
        if path == filter
            || (path.starts_with(filter) && path.as_bytes().get(filter.len()) == Some(&b'/'))
        {
            return true;
        }
        // Case-insensitive prefix match on Windows.
        #[cfg(target_os = "windows")]
        {
            let filter_lower = filter.to_ascii_lowercase();
            let path_lower = path.to_ascii_lowercase();
            if path.eq_ignore_ascii_case(filter)
                || (path_lower.starts_with(filter_lower.as_str())
                    && path_lower.as_bytes().get(filter_lower.len()) == Some(&b'/'))
            {
                return true;
            }
        }
        return false;
    }
    if path_matches_exact_or_suffix(path, filter) {
        return true;
    }
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
    // Same trim policy as `symbol_json`: drop `freshness` (always "Fresh" in
    // steady state) and emit `visibility`/`dirty` only when set. `signature`
    // stays because `diff_context` lists changed symbols and the model needs
    // it to triage them without an extra `read_slice`.
    let mut object = serde_json::Map::with_capacity(11);
    object.insert("id".to_string(), json!(symbol.id.0));
    object.insert("name".to_string(), json!(symbol.name));
    object.insert("kind".to_string(), json!(format!("{:?}", symbol.kind)));
    object.insert("path".to_string(), json!(symbol.file_id.0));
    object.insert("signature".to_string(), json!(symbol.signature));
    object.insert("span".to_string(), span_json(symbol.span));
    if let Some(visibility) = symbol.visibility.as_deref() {
        object.insert("visibility".to_string(), json!(visibility));
    }
    if let Some(dirty) = symbol.dirty.as_ref() {
        object.insert(
            "dirty".to_string(),
            json!({
                "status": dirty.status,
                "ranges": dirty.ranges.iter().map(|range| json!({
                    "start_line": range.start_line,
                    "end_line": range.end_line,
                })).collect::<Vec<_>>(),
            }),
        );
    }
    object.insert("references".to_string(), json!(references));
    object.insert("callers".to_string(), json!(callers));
    object.insert("diagnostics".to_string(), json!(diagnostics));
    object.insert("confidence".to_string(), json!(symbol.confidence.id()));
    Value::Object(object)
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

pub(crate) fn graph_unavailable_result(call: &ToolCall, still_indexing: bool) -> ToolResult {
    graph_unavailable_result_with_error(call, still_indexing, None)
}

/// Like [`graph_unavailable_result`] but surfaces a `store_open_error` key
/// when `open_error` is `Some`, so the model can distinguish a persistence
/// failure (which leaves the graph structurally absent but recoverable on the
/// next process start) from a workspace that genuinely has no graph.
pub(crate) fn graph_unavailable_result_with_error(
    call: &ToolCall,
    still_indexing: bool,
    open_error: Option<String>,
) -> ToolResult {
    // `fallback.suggested_tools` is intentionally absent — the reason code is
    // enough for the model to pick a non-graph retry path on its own.
    //
    // Three cases:
    //   still_indexing=true → graph is building; retry shortly
    //   still_indexing=false + open_error=Some → store/parse failure on open;
    //     the graph is absent because of an error, not by design
    //   still_indexing=false + open_error=None → workspace has no graph
    let (status, reason, retryable) = if still_indexing {
        (
            "graph_indexing",
            "semantic graph is still being indexed; retry this tool call",
            true,
        )
    } else if open_error.is_some() {
        (
            "graph_open_error",
            "semantic graph is unavailable: the graph open failed (see store_open_error)",
            false,
        )
    } else {
        (
            "graph_unavailable",
            "semantic graph is unavailable for this workspace",
            false,
        )
    };
    let mut body = serde_json::Map::new();
    body.insert("tool".to_string(), json!(call.name));
    body.insert("graph_available".to_string(), json!(false));
    body.insert("reason".to_string(), json!(reason));
    body.insert("packets".to_string(), json!([]));
    body.insert(
        "fallback".to_string(),
        json!({
            "status": status,
            "retryable": retryable,
        }),
    );
    if let Some(err) = open_error {
        body.insert("store_open_error".to_string(), json!(err));
    }
    make_result(
        call,
        ToolStatus::Success,
        Value::Object(body),
        ToolCostHint::default(),
        None,
    )
}

pub(crate) fn graph_payload(
    tool: &str,
    manager: &GraphManager,
    refresh: &squeezy_graph::RefreshReport,
) -> serde_json::Map<String, Value> {
    // Trim policy: `refresh` and `coverage` are dropped from the wire payload.
    // The model never branched on either; the byte cost ran ~150-400B per
    // graph tool result. Both signals still flow to telemetry via the typed
    // graph events emitted around `refresh_before_query()`.
    let mut payload = serde_json::Map::new();
    payload.insert("tool".to_string(), json!(tool));
    payload.insert("graph_available".to_string(), json!(true));
    payload.insert(
        "freshness_mode".to_string(),
        json!(manager.freshness_mode().as_str()),
    );
    if let Some(reason) = manager.freshness_fallback_reason() {
        payload.insert("freshness_fallback_reason".to_string(), json!(reason));
    }
    let indexing_decision = &manager.build_report().indexing_decision;
    if !indexing_decision.should_index {
        payload.insert(
            "indexing_decision".to_string(),
            json!({
                "should_index": false,
                "reason": &indexing_decision.reason,
                "positive_signals": &indexing_decision.positive_signals,
                "negative_signals": &indexing_decision.negative_signals,
            }),
        );
    }
    // Bug #2: when the incremental refresh budget was exhausted, some changed
    // files were never reparsed and stay queued for the next refresh — the
    // graph evidence below is partially stale. A bare `graph_available=true`
    // hides that, so surface a compact `refresh_incomplete` signal plus the
    // count of still-pending changed paths (read from the manager, since the
    // unprocessed paths are left queued in that case). Emitted only when the
    // refresh did not fully complete, so a healthy graph pays no byte cost.
    // Capture once: acquiring the mutex twice for the same counter would be
    // redundant and could produce inconsistent values if a watcher thread
    // enqueues a path between the two calls.
    let pending_events = manager.pending_changed_count();
    if refresh.budget_exhausted {
        payload.insert("refresh_incomplete".to_string(), json!(true));
        payload.insert("stale_pending".to_string(), json!(pending_events));
    }
    let watcher = manager.watcher_status();
    // Mirror the `refresh_incomplete` gating above: a healthy graph pays no
    // byte cost on the wire. Emit the `indexing` block only when the
    // session is in a degraded state the model should know about — polling
    // fallback engaged, pending events queued, or any captured
    // fallback_reason. Healthy native sessions and one-shot CLI invocations
    // (mode = Disabled with no fallback reason) skip the ~80-110 bytes of
    // stable JSON entirely.
    let watcher_degraded = matches!(watcher.mode, squeezy_graph::WatcherMode::PollingFallback)
        || pending_events > 0
        || watcher.fallback_reason.is_some();
    if watcher_degraded {
        payload.insert(
            "indexing".to_string(),
            json!({
                "watcher_mode": watcher.mode.as_str(),
                "watcher_backend": watcher.backend,
                "pending_events": pending_events,
                "fallback_reason": watcher.fallback_reason,
            }),
        );
    }
    // Surface unmatched watcher events: events that were pending but did not
    // correspond to any changed or removed file in the crawl. On Linux these
    // are a practical signal for path-spelling, symlink, or bind-mount
    // mismatches in event reconciliation.
    if refresh.unchanged_event_paths > 0 {
        payload.insert(
            "unmatched_watcher_events".to_string(),
            json!(refresh.unchanged_event_paths),
        );
    }
    if !refresh.path_conflicts.is_empty() {
        payload.insert(
            "path_conflicts".to_string(),
            path_conflicts_json(&refresh.path_conflicts),
        );
    }
    payload
}

fn path_conflicts_json(conflicts: &[squeezy_workspace::PathConflict]) -> Value {
    json!({
        "count": conflicts.len(),
        "samples": conflicts.iter().take(5).map(|conflict| {
            json!({
                "normalized_relative_path": &conflict.normalized_relative_path,
                "relative_paths": &conflict.relative_paths,
            })
        }).collect::<Vec<_>>(),
    })
}

fn graph_stats_json(graph: &squeezy_graph::SemanticGraph) -> Value {
    let stats = graph.stats();
    let mut obj = json!({
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
    });
    // Surface case-collision count only when non-zero so the common-case
    // response pays no byte cost. Non-zero means a Windows checkout produced
    // two differently-cased spellings for the same logical file.
    if stats.case_collision_count > 0
        && let Some(map) = obj.as_object_mut()
    {
        map.insert(
            "case_collision_count".to_string(),
            json!(stats.case_collision_count),
        );
    }
    obj
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
    let mut map = serde_json::Map::new();
    map.insert("level".to_string(), json!(diagnostic.level));
    map.insert("message".to_string(), json!(diagnostic.message));
    map.insert("code".to_string(), json!(diagnostic.code));
    map.insert(
        "path".to_string(),
        json!(diagnostic.file_id.as_ref().map(|id| id.0.clone())),
    );
    map.insert("span".to_string(), json!(diagnostic.span.map(span_json)));
    map.insert("label".to_string(), json!(diagnostic.label));
    map.insert("package_id".to_string(), json!(diagnostic.package_id));
    map.insert("target_name".to_string(), json!(diagnostic.target_name));
    map.insert(
        "freshness".to_string(),
        cargo_freshness_json(&hit.freshness),
    );
    map.insert(
        "provenance".to_string(),
        provenance_json(diagnostic.provenance.clone()),
    );
    // When the compiler path could not be mapped to a workspace-relative
    // FileId, include the raw path so users can diagnose container, symlink,
    // or bind-mount spelling mismatches between cargo's output and the
    // workspace root squeezy was opened with.
    if let Some(raw) = &diagnostic.raw_path {
        map.insert("raw_path".to_string(), json!(raw));
        map.insert(
            "raw_path_hint".to_string(),
            json!("path not matched to workspace; check for container/symlink/bind-mount mismatch"),
        );
    }
    Value::Object(map)
}

fn graph_language_counts_json(graph: &squeezy_graph::SemanticGraph) -> Value {
    graph_language_counts_scoped_json(graph, None, None)
}

/// Per-language file counts, optionally restricted to a `path` subtree and/or a
/// single `language`. Used by `repo_map` so its coverage map reflects the same
/// scope as its hierarchy when the caller narrows by path/language. Empty
/// filters fall through to the whole-graph count.
fn graph_language_counts_scoped_json(
    graph: &squeezy_graph::SemanticGraph,
    path: Option<&str>,
    language: Option<&str>,
) -> Value {
    let path = path.map(str::trim).filter(|value| !value.is_empty());
    let language = language.map(str::trim).filter(|value| !value.is_empty());
    let mut counts = BTreeMap::<&'static str, usize>::new();
    for file in graph.files.values() {
        if let Some(path) = path
            && !path_matches_filter(file.relative_path.as_str(), path)
        {
            continue;
        }
        if !file_language_matches(graph, &file.id, language) {
            continue;
        }
        *counts.entry(file.language.display_name()).or_default() += 1;
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

/// Split a possibly pipe-separated `kind` argument into its individual,
/// trimmed, non-empty tokens (e.g. `"struct|enum|trait"` → three tokens).
/// Returns `None` when no kind was supplied or every token is blank, so callers
/// fall back to their existing single-kind / no-kind path. A single-valued kind
/// yields a one-element vec, preserving the original behavior.
fn split_kind_tokens(kind: Option<&str>) -> Option<Vec<&str>> {
    let kind = kind?;
    let tokens: Vec<&str> = kind
        .split('|')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens)
    }
}

fn single_symbol_kind(filter: Option<SymbolKindFilter>) -> Option<SymbolKind> {
    match filter {
        Some(SymbolKindFilter::Single(kind)) => Some(kind),
        _ => None,
    }
}

/// Run a single-kind symbol search once per pipe-separated `kind` token and
/// union the results, deduplicating by symbol id while preserving first-seen
/// order. A `None`/single-token kind runs `search` exactly once (with the
/// original argument), so the no-kind and single-kind paths are unchanged. This
/// is how the multi-valued `kind` ("struct|enum|trait") matcher reuses the
/// existing single-kind search instead of teaching the matcher about lists.
fn multi_kind_symbol_union(
    kind: Option<&str>,
    mut search: impl FnMut(Option<&str>) -> Vec<GraphSymbol>,
) -> Vec<GraphSymbol> {
    match split_kind_tokens(kind) {
        Some(tokens) if tokens.len() > 1 => {
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for token in tokens {
                for symbol in search(Some(token)) {
                    if seen.insert(symbol.id.clone()) {
                        out.push(symbol);
                    }
                }
            }
            out
        }
        // No kind, a single token, or an all-blank kind: one pass with the
        // original argument so trigram seeding and ranking are untouched.
        _ => search(kind),
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
    attribute_filter_matches(&symbol.attributes, attribute)
}

/// Multi-value attribute matching: a `|`-separated filter (e.g.
/// `base:A|base:B|base:C`) matches if the symbol satisfies ANY alternative.
/// This lets an "enumerate symbols whose base is one of N" query be a SINGLE
/// `decl_search` call instead of N — the difference between staying under the
/// per-turn tool-call budget and starving it on a wide hierarchy.
///
/// Each alternative is matched against an attribute with case-insensitive
/// EQUALITY only. A substring fallback was a false-positive source: filter
/// `base:User` would wrongly match a symbol carrying `base:UserProfile`.
/// Exact, segmented attributes (`mixin:ns:leaf`) still match because the
/// stored attribute string is compared in full.
fn attribute_filter_matches(attributes: &[String], filter: &str) -> bool {
    filter
        .split('|')
        .map(str::trim)
        .filter(|alternative| !alternative.is_empty())
        .any(|alternative| {
            attributes
                .iter()
                .any(|value| value.eq_ignore_ascii_case(alternative))
        })
}

pub(crate) fn graph_symbol_search(
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
    // No query (and no dotted hits): leave `candidates` as `None` so the caller
    // filters over borrowed values and clones only the survivors rather than
    // cloning the whole symbol table up front.
    let candidates: Option<Vec<GraphSymbol>> = if let Some(matches) = dotted_hits {
        Some(matches)
    } else {
        query.map(|query| {
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
        })
    };
    let mut symbols = match candidates {
        Some(candidates) => candidates
            .into_iter()
            .filter(|symbol| seen.insert(symbol.id.clone()))
            .filter(|symbol| symbol_matches_kind_filter(symbol.kind, kind_filter))
            .filter(|symbol| symbol_matches_visibility_filter(symbol, visibility))
            .filter(|symbol| symbol_matches_attribute_filter(symbol, attribute))
            .filter(|symbol| symbol_matches_path_filter(symbol, path))
            .filter(|symbol| language_matches(graph, symbol, language))
            .collect::<Vec<_>>(),
        None => graph
            .symbols
            .values()
            .filter(|symbol| seen.insert(symbol.id.clone()))
            .filter(|symbol| symbol_matches_kind_filter(symbol.kind, kind_filter))
            .filter(|symbol| symbol_matches_visibility_filter(symbol, visibility))
            .filter(|symbol| symbol_matches_attribute_filter(symbol, attribute))
            .filter(|symbol| symbol_matches_path_filter(symbol, path))
            .filter(|symbol| language_matches(graph, symbol, language))
            .cloned()
            .collect::<Vec<_>>(),
    };

    // Fuzzy widening: when the trigram-anchored candidate pool is empty
    // but a query was provided, run a fuzzy subsequence scan over a
    // bounded candidate set so casual queries (`graphmgr → GraphManager`)
    // still resolve. This only runs on a miss so high-confidence behaviour
    // is unchanged.
    //
    // Prefilter: when the query has 3+ chars, use the first trigram as a
    // seed for `signature_search` to leverage the trigram index and avoid
    // scanning every symbol with the full fuzzy algorithm. If the seed cannot
    // produce a ranked match, fall back to the full symbol map to preserve the
    // previous fuzzy recall.
    if symbols.is_empty()
        && let Some(query) = query
    {
        let fuzzy_matches = |candidates: Vec<GraphSymbol>| {
            candidates
                .into_iter()
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
                .collect::<Vec<_>>()
        };
        let mut used_seed_hits = false;
        let candidates: Vec<GraphSymbol> = if query.len() >= 3 {
            let seed_end = query.char_indices().nth(3).map_or(query.len(), |(i, _)| i);
            let seed_hits = graph.signature_search(&SignatureQuery {
                text: query[..seed_end].to_string(),
                kind: None,
                visibility: None,
                attribute: None,
            });
            if seed_hits.is_empty() {
                graph.symbols.values().cloned().collect()
            } else {
                used_seed_hits = true;
                seed_hits
            }
        } else {
            graph.symbols.values().cloned().collect()
        };
        symbols = fuzzy_matches(candidates);
        if symbols.is_empty() && used_seed_hits {
            symbols = fuzzy_matches(graph.symbols.values().cloned().collect());
        }
    }

    // Precompute per-symbol sort keys once to avoid re-tokenizing the query
    // inside the O(n log n) comparator. BM25 scores are computed here too
    // so they can act as a within-tier tiebreaker without crossing tier
    // boundaries.
    //
    // Sort key tuple: (tier, bm25_neg, lex_score, path_key)
    //   tier      — lower value = better rank tier (usize)
    //   bm25_neg  — negated fixed-point BM25 score (i64); symbols with
    //               positive BM25 sort before zero-score ones within a tier;
    //               zero when BM25 is not applicable
    //   lex_score — fuzzy/token lexical score (i32); lower = better
    //   path_key  — PathRank::sort_key() (i32, i32, i32); lower = better
    type SortKey = (usize, i64, i32, (i32, i32, i32));

    let bm25_scores: Vec<f32> = if let Some(query) = query
        && query.split_whitespace().count() >= 2
        && symbols.len() > 1
    {
        let doc_bufs: Vec<(String, String)> = symbols
            .iter()
            .map(|sym| (sym.docs.join(" "), sym.attributes.join(" ")))
            .collect();
        let bm25_docs: Vec<squeezy_rank::BM25Doc<'_>> = symbols
            .iter()
            .zip(doc_bufs.iter())
            .map(|(sym, (docs, attrs))| squeezy_rank::BM25Doc {
                signature: sym.signature.as_str(),
                docs: docs.as_str(),
                attributes: attrs.as_str(),
            })
            .collect();
        let reranked = squeezy_rank::bm25_rerank(&bm25_docs, query, symbols.len());
        let mut scores = vec![0.0f32; symbols.len()];
        for (idx, score) in reranked {
            scores[idx] = score;
        }
        scores
    } else {
        vec![0.0; symbols.len()]
    };

    let sort_keys: Vec<SortKey> = symbols
        .iter()
        .zip(bm25_scores.iter())
        .map(|(sym, &bm25)| {
            let (tier, lex_score) = query
                .map(|q| symbol_rank(sym, q))
                .unwrap_or((usize::MAX, 0));
            let path_key = path
                .map(|p| squeezy_rank::path_rank::path_rank(&sym.file_id.0, p).sort_key())
                .unwrap_or((i32::MAX, i32::MAX, i32::MAX));
            // Negate BM25 so higher score sorts first; 0.0 → 0 sorts after
            // any positive-BM25 symbol.
            let bm25_neg = -(bm25 * 1000.0).round() as i64;
            (tier, bm25_neg, lex_score, path_key)
        })
        .collect();

    let original = std::mem::take(&mut symbols);
    let mut indices: Vec<usize> = (0..original.len()).collect();
    indices.sort_by(|&i, &j| {
        sort_keys[i]
            .cmp(&sort_keys[j])
            .then(original[i].file_id.0.cmp(&original[j].file_id.0))
            .then(
                original[i]
                    .span
                    .start_byte
                    .cmp(&original[j].span.start_byte),
            )
    });
    symbols = indices.into_iter().map(|i| original[i].clone()).collect();

    symbols
}

/// Cap on the number of symbols a transitive subtype closure may return.
/// Keeps a deep or wide hierarchy from producing an unbounded payload; the
/// seen-names set guarantees termination, this guarantees a bounded size.
pub(crate) const TRANSITIVE_CLOSURE_CAP: usize = 200;

/// Inheritance-attribute prefixes the transitive closure understands. A seed
/// `attribute` value carrying any of these enumerates subtypes by name.
const INHERITANCE_PREFIXES: [&str; 3] = ["base:", "mixin:", "iface:"];

/// True when `attribute` carries at least one inheritance prefix
/// (`base:`/`mixin:`/`iface:`), i.e. the filter names a supertype to enumerate
/// subtypes of. Only then does a transitive closure make sense.
fn attribute_has_inheritance_prefix(attribute: &str) -> bool {
    attribute
        .split('|')
        .map(str::trim)
        .any(|alt| INHERITANCE_PREFIXES.iter().any(|p| alt.starts_with(p)))
}

/// Parse the seed supertype name(s) out of an inheritance `attribute` filter.
/// Each `prefix:Name` alternative contributes `Name`; prefix-free or empty
/// alternatives are skipped. De-duplicates while preserving first-seen order.
fn seed_type_names(attribute: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    for alt in attribute.split('|').map(str::trim) {
        for prefix in INHERITANCE_PREFIXES {
            if let Some(name) = alt.strip_prefix(prefix) {
                let name = name.trim();
                if !name.is_empty() && seen.insert(name.to_string()) {
                    names.push(name.to_string());
                }
                break;
            }
        }
    }
    names
}

/// Transitive subtype closure for an inheritance `attribute` filter.
///
/// `decl_search`/grep's direct-attribute filter only ever surfaces the
/// *immediate* subtypes of a base (`class B extends A` records `base:A` on
/// `B`, but `class C extends B` records `base:B`, not `base:A`). This walks the
/// hierarchy by name: starting from each seed supertype, it repeatedly runs the
/// existing direct-attribute search (`base:N|mixin:N|iface:N`) and enqueues
/// every newly discovered subtype's name, so an `attribute="base:A"` query
/// returns A's whole subtype tree (B, C, ...), not just B.
///
/// Termination/cycle safety comes from a seen-names set (each type name is
/// expanded at most once); size is bounded by [`TRANSITIVE_CLOSURE_CAP`]. The
/// `path`/`language`/`visibility` scope the walk; `kind` and `query` are
/// applied to the *emitted* results (not the walk) so a mixed-kind intermediate
/// is still traversed and a `query` still narrows the closure — matching the
/// non-transitive search's filter semantics.
// Mirrors `graph_symbol_search`'s filter surface (query/kind/path/language/
// visibility) plus the seed/cap, so the argument count is inherent.
#[allow(clippy::too_many_arguments)]
pub(crate) fn graph_transitive_subtype_closure(
    graph: &squeezy_graph::SemanticGraph,
    query: Option<&str>,
    kind: Option<&str>,
    path: Option<&str>,
    language: Option<&str>,
    visibility: Option<&str>,
    seed_names: &[String],
    cap: usize,
) -> (Vec<GraphSymbol>, bool) {
    let kind_filter = kind.and_then(parse_symbol_kind_filter);
    let query = query.map(str::trim).filter(|value| !value.is_empty());

    let mut seen_names: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    for name in seed_names {
        if seen_names.insert(name.clone()) {
            queue.push_back(name.clone());
        }
    }

    let mut seen_ids: HashSet<SymbolId> = HashSet::new();
    let mut results: Vec<GraphSymbol> = Vec::new();

    // Hoist a single borrowed snapshot scoped by the constant walk filters
    // (`path`/`language`/`visibility`) once, instead of re-scanning and cloning
    // the whole symbol table inside `graph_symbol_search` for every BFS node.
    // Only the per-node attribute filter varies, so it's applied below over
    // references and survivors are cloned.
    let scope: Vec<&GraphSymbol> = graph
        .symbols
        .values()
        .filter(|symbol| symbol_matches_visibility_filter(symbol, visibility))
        .filter(|symbol| symbol_matches_path_filter(symbol, path))
        .filter(|symbol| language_matches(graph, symbol, language))
        .collect();

    while let Some(name) = queue.pop_front() {
        let attribute = format!("base:{name}|mixin:{name}|iface:{name}");
        // Expand KIND-AGNOSTICALLY. The hierarchy must be walked through every
        // inheritance edge regardless of kind, or a mixed-kind *intermediate*
        // (e.g. `interface J extends I`, an abstract class between two concrete
        // ones) is never enqueued and its whole subtree is silently dropped
        // (`class C implements J` would be missed for a `kind=class base:I`
        // query). The requested `kind` is applied to *emitted* results below,
        // not to the walk. `path`/`language`/`visibility` already scoped the
        // hoisted snapshot above.
        let matches = scope
            .iter()
            .copied()
            .filter(|symbol| symbol_matches_attribute_filter(symbol, Some(&attribute)));
        for symbol in matches {
            let newly_seen = seen_ids.insert(symbol.id.clone());
            // Enqueue every newly-seen *name* so its subtypes are discovered too
            // — even when the symbol itself is filtered out of the results. This
            // is what keeps the closure transitive across kind boundaries.
            if seen_names.insert(symbol.name.clone()) {
                queue.push_back(symbol.name.clone());
            }
            if !newly_seen {
                continue;
            }
            // Result filters. `kind` narrows emitted results (the walk above was
            // kind-agnostic); `query`, when present, narrows the closure to name
            // matches so `decl_search{query,attribute:base:X,transitive:true}`
            // honors the query just like the non-transitive path does.
            if !symbol_matches_kind_filter(symbol.kind, kind_filter) {
                continue;
            }
            if let Some(query) = query {
                let view = squeezy_rank::GraphSymbolView {
                    name: symbol.name.as_str(),
                    signature: symbol.signature.as_str(),
                };
                if squeezy_rank::symbol_rank::rank_symbol(view, query).0
                    == squeezy_rank::symbol_rank::RankTier::NoMatch
                {
                    continue;
                }
            }
            if results.len() >= cap {
                // Closure hit the cap: signal that the subtype tree was
                // truncated so callers can surface it distinctly.
                return (results, true);
            }
            // Only survivors are cloned out of the borrowed snapshot.
            results.push(symbol.clone());
        }
    }

    (results, false)
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
    let segments: Vec<&str> = query
        .split("::")
        .flat_map(|segment| segment.split('.'))
        .filter(|segment| !segment.is_empty())
        .collect();
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
    member_candidates.retain(|cand| {
        let mut want = segments[..segments.len() - 1].iter().rev().peekable();
        let mut current = cand.parent_id.as_ref();
        while let Some(needed) = want.peek() {
            let Some(parent_id) = current else {
                return false;
            };
            let Some(parent) = graph.symbols.get(parent_id) else {
                return false;
            };
            if parent.name.as_str() == **needed {
                want.next();
            }
            current = parent.parent_id.as_ref();
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
    let mut counts = BTreeMap::<&'static str, usize>::new();
    for symbol in symbols {
        let label = graph
            .files
            .get(&symbol.file_id)
            .map(|file| file.language.display_name())
            .unwrap_or("unknown");
        *counts.entry(label).or_default() += 1;
    }
    json!(counts)
}

fn decl_counts_by_kind(symbols: &[GraphSymbol]) -> Value {
    let mut counts = BTreeMap::<&'static str, usize>::new();
    for symbol in symbols {
        *counts.entry(symbol_kind_label(symbol.kind)).or_default() += 1;
    }
    json!(counts)
}

/// True when a path looks like test code. Recognises the common per-language
/// conventions: a `test`/`tests`/`__tests__`/`spec`/`testing` directory
/// segment, or a file name ending in a test/spec suffix
/// (`_test`/`_tests`/`.test`/`.spec`/`Test`/`Tests`/`Spec`). Used by the
/// `exclude_tests`/`tests_only` scoping shared across the read tools.
fn path_is_test(path: &str) -> bool {
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
fn symbol_is_test(symbol: &GraphSymbol) -> bool {
    symbol.kind == SymbolKind::Test || path_is_test(symbol.file_id.0.as_str())
}

/// Apply an `exclude_tests`/`tests_only` pair to a test-ness verdict. When both
/// are set, `tests_only` wins (the more specific request). Returns whether the
/// item should be KEPT.
fn passes_test_scope(is_test: bool, exclude_tests: bool, tests_only: bool) -> bool {
    if tests_only {
        is_test
    } else if exclude_tests {
        !is_test
    } else {
        true
    }
}

/// Parse an `edge_kind` filter token to an [`EdgeKind`]. Accepts the
/// case-insensitive names the flow/symbol_context tools advertise:
/// `calls`/`references`/`imports`/`reexports`. Returns `None` for anything
/// else so an unknown token is treated as "no filter" by the caller.
fn parse_edge_kind_filter(value: &str) -> Option<EdgeKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "calls" | "call" => Some(EdgeKind::Calls),
        "references" | "reference" | "ref" => Some(EdgeKind::References),
        "imports" | "import" => Some(EdgeKind::Imports),
        "reexports" | "reexport" | "re-export" => Some(EdgeKind::Reexports),
        _ => None,
    }
}

/// True when a result packet's `edge.kind` matches the requested edge kind.
/// Packets that carry no `edge` body (pure symbol/reference packets) are kept
/// only when no edge-kind filter is active — an edge-kind filter is meaningful
/// only for edge-bearing packets, so a `reference`/`symbol` packet is dropped
/// when the caller asked for a specific edge kind.
fn packet_matches_edge_kind(packet: &Value, want: Option<EdgeKind>) -> bool {
    let Some(want) = want else {
        return true;
    };
    let want_label = format!("{want:?}");
    packet
        .get("edge")
        .and_then(|edge| edge.get("kind"))
        .and_then(Value::as_str)
        .map(|kind| kind == want_label)
        .unwrap_or(false)
}

/// Best-effort extraction of the workspace-relative path a result packet points
/// at, for the `result_path` filter on the flow/hierarchy/symbol_context tools.
/// Looks at the packet bodies these tools emit: `symbol.path`, `reference.path`,
/// the first `spans[].path`, and (for edge packets) the `edge.from` symbol's
/// file via the `spans` entry. Returns `None` when no path can be determined.
fn packet_path(packet: &Value) -> Option<&str> {
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
fn packet_matches_result_path(packet: &Value, filter: Option<&str>) -> bool {
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
fn result_path_scope<'a>(path: Option<&'a str>, result_path: Option<&'a str>) -> Option<&'a str> {
    result_path
        .or(path)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// True when a result packet passes the `exclude_tests`/`tests_only` scope,
/// keyed on the packet's path (see [`packet_path`]/[`path_is_test`]). Packets
/// with no determinable path are KEPT under `exclude_tests` and DROPPED under
/// `tests_only` (a path-less packet can't be confirmed as a test).
fn packet_matches_test_scope(packet: &Value, exclude_tests: bool, tests_only: bool) -> bool {
    if !exclude_tests && !tests_only {
        return true;
    }
    match packet_path(packet) {
        Some(path) => passes_test_scope(path_is_test(path), exclude_tests, tests_only),
        None => !tests_only,
    }
}

/// Edge kinds that count as a "use" of a declaration for dead-code analysis:
/// a resolved call site, a textual/identifier reference, or a test exercising
/// it. Inbound `Contains`/`Imports`/inheritance edges are structural, not uses,
/// so they are excluded.
fn is_usage_edge(kind: EdgeKind) -> bool {
    matches!(
        kind,
        EdgeKind::Calls | EdgeKind::References | EdgeKind::TestOf
    )
}

/// Count inbound usage edges (`Calls`/`References`/`TestOf`) pointing at this
/// symbol via the graph's `edges_by_to` index.
fn inbound_usage_count(graph: &squeezy_graph::SemanticGraph, symbol: &GraphSymbol) -> usize {
    graph
        .inbound_edges(&symbol.id)
        .filter(|edge| is_usage_edge(edge.kind))
        .count()
}

/// True when the symbol's visibility marks it as part of a public/exported
/// surface. Such a declaration may have callers outside the scanned graph
/// (downstream crates, reflection, FFI), so dead-code mode FLAGS it rather than
/// asserting it is dead. Treats anything that is not explicitly
/// `private`/`protected`/`internal`/`fileprivate` as exported, matching the
/// graph's own permissive export heuristic.
fn visibility_is_exported(symbol: &GraphSymbol) -> bool {
    match symbol.visibility.as_deref() {
        None => false,
        Some(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "private" | "protected" | "internal" | "fileprivate" | "module" | "package"
        ),
    }
}

/// Confidence levels at which "zero inbound usage edges" is NOT trustworthy
/// evidence of dead code: a macro-opaque/conditional/external/candidate-set
/// declaration may well be used through an edge the resolver could not pin
/// down. Such candidates are caveated, never reported as confidently dead.
fn confidence_is_unresolved(confidence: Confidence) -> bool {
    matches!(
        confidence,
        Confidence::MacroOpaque
            | Confidence::ConditionalUnknown
            | Confidence::External
            | Confidence::CandidateSet
    )
}

/// Parse a confidence-level filter token to a [`Confidence`]. Accepts the
/// stable snake_case ids ([`Confidence::id`]) case-insensitively, plus a couple
/// of obvious aliases. Returns `None` for an unknown token so the caller treats
/// it as "no constraint" rather than silently dropping everything.
fn parse_confidence_level(value: &str) -> Option<Confidence> {
    match value.trim().to_ascii_lowercase().as_str() {
        "exact_syntax" | "exact" => Some(Confidence::ExactSyntax),
        "import_resolved" | "resolved" => Some(Confidence::ImportResolved),
        "heuristic" => Some(Confidence::Heuristic),
        "candidate_set" | "candidate" => Some(Confidence::CandidateSet),
        "external" => Some(Confidence::External),
        "macro_opaque" | "macro" => Some(Confidence::MacroOpaque),
        "conditional_unknown" | "conditional" => Some(Confidence::ConditionalUnknown),
        "unsupported" => Some(Confidence::Unsupported),
        "stale" => Some(Confidence::Stale),
        "partial" => Some(Confidence::Partial),
        _ => None,
    }
}

/// Confidence-scoping options shared by the read tools (O10). `levels` is an
/// optional allow-set parsed from a pipe-separated `confidence` argument;
/// `exclude_external` drops out-of-workspace (`External`) targets; `external_only`
/// keeps only them (and wins over `exclude_external` when both are set).
#[derive(Debug)]
struct ConfidenceScope {
    levels: Option<Vec<Confidence>>,
    exclude_external: bool,
    external_only: bool,
}

impl ConfidenceScope {
    /// Build from the raw args. An all-unknown / blank `confidence` string yields
    /// no level constraint so a typo never empties the result silently.
    fn new(confidence: Option<&str>, exclude_external: bool, external_only: bool) -> Self {
        let levels = confidence.and_then(|raw| {
            let parsed: Vec<Confidence> = raw
                .split('|')
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .filter_map(parse_confidence_level)
                .collect();
            (!parsed.is_empty()).then_some(parsed)
        });
        Self {
            levels,
            exclude_external,
            external_only,
        }
    }

    /// True when no constraint is active, so callers can skip the filter pass.
    fn is_noop(&self) -> bool {
        self.levels.is_none() && !self.exclude_external && !self.external_only
    }

    /// Whether a target with this confidence is kept under the scope.
    fn keeps(&self, confidence: Confidence) -> bool {
        if self.external_only {
            return confidence == Confidence::External;
        }
        if self.exclude_external && confidence == Confidence::External {
            return false;
        }
        match &self.levels {
            Some(levels) => levels.contains(&confidence),
            None => true,
        }
    }

    /// Packet-level variant for JSON packets (flows): reads the `confidence` id
    /// from the packet's `symbol`/`reference`/`edge` body. A packet with no
    /// determinable confidence is KEPT (the scope is a positive filter, not a
    /// hard gate that would drop un-anchored packets).
    fn keeps_packet(&self, packet: &Value) -> bool {
        if self.is_noop() {
            return true;
        }
        match packet_confidence_id(packet) {
            Some(id) => match parse_confidence_level(id) {
                Some(confidence) => self.keeps(confidence),
                None => true,
            },
            None => true,
        }
    }
}

/// Best-effort extraction of a packet's confidence id from the bodies the graph
/// tools emit (`symbol`/`reference`/`edge`). Returns `None` when no confidence
/// can be located.
fn packet_confidence_id(packet: &Value) -> Option<&str> {
    for body in ["symbol", "reference", "edge"] {
        if let Some(id) = packet
            .get(body)
            .and_then(|value| value.get("confidence"))
            .and_then(Value::as_str)
        {
            return Some(id);
        }
    }
    packet.get("confidence").and_then(Value::as_str)
}

pub(crate) fn resolve_definition_candidates(
    graph: &squeezy_graph::SemanticGraph,
    symbol_id: Option<&str>,
    query: Option<&str>,
    kind: Option<&str>,
    path: Option<&str>,
    language: Option<&str>,
) -> Vec<GraphSymbol> {
    // A supplied symbol_id is authoritative *only while it's live*: a stale id
    // (graph re-indexed since it was minted) must fall through to the
    // query/kind/path search rather than dead-ending on an empty result.
    if let Some(symbol_id) = symbol_id
        && let Some(symbol) = graph.symbols.get(&SymbolId::new(symbol_id))
    {
        return vec![symbol.clone()];
    }
    let Some(query) = query else {
        return Vec::new();
    };
    graph_symbol_search(graph, Some(query), kind, path, language, None, None)
}

fn symbol_rank(symbol: &GraphSymbol, query: &str) -> (usize, i32) {
    // Preserve the historical exact > case-insensitive > signature-substring
    // ordering. `squeezy_rank` adds two extra tiers (token-bag, fuzzy) that
    // recover near-miss queries like `graphmgr → GraphManager` without
    // changing the relative ordering of existing high-confidence hits.
    // The lexical score is kept as a secondary key so two fuzzy matches are
    // ordered by closeness rather than file path.
    let view = squeezy_rank::GraphSymbolView {
        name: symbol.name.as_str(),
        signature: symbol.signature.as_str(),
    };
    let (tier, score) = squeezy_rank::symbol_rank::rank_symbol(view, query);
    (tier.as_usize(), score)
}

fn symbol_rank_label(symbol: &GraphSymbol, query: &str) -> &'static str {
    let view = squeezy_rank::GraphSymbolView {
        name: symbol.name.as_str(),
        signature: symbol.signature.as_str(),
    };
    match squeezy_rank::symbol_rank::rank_symbol(view, query).0 {
        squeezy_rank::RankTier::Exact => "exact",
        squeezy_rank::RankTier::CaseInsensitive => "case_insensitive",
        squeezy_rank::RankTier::SignatureSubstring => "signature_substring",
        squeezy_rank::RankTier::TokenBag => "token_bag",
        squeezy_rank::RankTier::Fuzzy => "fuzzy",
        squeezy_rank::RankTier::NoMatch => "no_match",
    }
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

pub(crate) fn symbol_kind_label(kind: SymbolKind) -> &'static str {
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
    file_language_matches(graph, &symbol.file_id, language)
}

/// Language predicate keyed directly on a `FileId`. Shared by `language_matches`
/// (symbol-based) and by tools whose results are keyed on a file rather than a
/// symbol (reference hits, importer files). Returns `true` when no language
/// filter is set; `false` when the file is unknown to the graph.
fn file_language_matches(
    graph: &squeezy_graph::SemanticGraph,
    file_id: &FileId,
    language: Option<&str>,
) -> bool {
    let Some(language) = language.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    let Some(file) = graph.files.get(file_id) else {
        return false;
    };
    let language = language.to_ascii_lowercase();
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
/// - `case_near_match`: nearest case-insensitive path candidate when reason
///   is `path_unknown` and a case-insensitive match exists; helps the model
///   correct a Linux case typo without a second graph traversal.
/// - `path_normalized_from`: original filter when backslash normalization
///   occurred; shows the slash-normalized path that was actually used.
///
/// `suggested_tools` is intentionally omitted from the wire payload —
/// recommending grep/decl_search retries was decoration the model already
/// knows how to do unprompted; the reason code is the load-bearing signal.
fn graph_zero_hit_fallback(
    graph: &squeezy_graph::SemanticGraph,
    symbol_id: Option<&str>,
    path: Option<&str>,
    _query: Option<&str>,
    packet_count: usize,
) -> Value {
    if packet_count > 0 {
        return Value::Null;
    }
    // A supplied-but-absent symbol_id is the most actionable zero-hit cause:
    // the id was minted against an earlier graph and is now stale. Surface that
    // distinctly so the caller re-resolves by name rather than rewording the
    // query (the misdirection the path/query reasons would otherwise give).
    if let Some(id) = symbol_id
        && !graph.symbols.contains_key(&SymbolId::new(id))
    {
        let mut obj = serde_json::Map::new();
        obj.insert("status".to_string(), json!("no_graph_evidence"));
        obj.insert("reason".to_string(), json!("symbol_id_stale"));
        obj.insert(
            "hint".to_string(),
            json!("re-resolve the symbol by name via definition_search; the supplied symbol_id is stale"),
        );
        obj.insert("symbol_id".to_string(), json!(id));
        return Value::Object(obj);
    }
    // Normalize backslashes once so all branches see forward-slash paths.
    // This mirrors path_matches_filter's normalization and ensures the file
    // lookup succeeds even when the caller supplied a Windows-style path.
    let path_norm_buf = path.map(|p| p.replace('\\', "/"));
    let normalized_path = path_norm_buf.as_deref();

    let (path_value, language_value, reason, hint, case_near_match) = match normalized_path {
        Some(p) => {
            let file = graph
                .files
                .values()
                .find(|file| path_matches_filter(&file.relative_path, p));
            #[cfg(target_os = "windows")]
            let file = file.or_else(|| graph.find_file_case_insensitive(p));
            match file {
                Some(file) => {
                    let (reason, hint) = match file.language {
                        LanguageKind::Unsupported => (
                            "path_unsupported",
                            "use grep or read_slice for this file type",
                        ),
                        LanguageKind::Unknown => (
                            "path_unknown",
                            "check the path spelling or use a broader search",
                        ),
                        _ => (
                            "supported_language_no_match",
                            "try a different query or broader kind filter",
                        ),
                    };
                    (
                        Value::String(file.relative_path.clone()),
                        Value::String(file.language.display_name().to_string()),
                        reason,
                        hint,
                        None,
                    )
                }
                None => {
                    // No exact/suffix match.  Try a case-insensitive search so
                    // the caller can correct a Linux case typo without another
                    // graph traversal.  Use min_by_key for a deterministic
                    // winner when a case-sensitive repo has multiple files that
                    // differ only by case (e.g. both `src/Foo.rs` and
                    // `src/foo.rs` match a query of `src/FOO.rs`).
                    let p_lower = p.to_lowercase();
                    let near = graph
                        .files
                        .values()
                        .filter(|f| {
                            let rp_lower = f.relative_path.to_lowercase();
                            rp_lower == p_lower
                                || rp_lower
                                    .strip_suffix(p_lower.as_str())
                                    .is_some_and(|prefix| prefix.ends_with('/'))
                        })
                        .min_by_key(|f| f.relative_path.as_str());
                    (
                        Value::String(p.to_string()),
                        Value::Null,
                        "path_unknown",
                        "check the path spelling or use a broader search",
                        near.map(|f| f.relative_path.clone()),
                    )
                }
            }
        }
        None => (
            Value::Null,
            Value::Null,
            "no_path_scope",
            "try a different query or broader kind filter",
            None,
        ),
    };

    let mut obj = serde_json::Map::new();
    obj.insert("status".to_string(), json!("no_graph_evidence"));
    obj.insert("reason".to_string(), json!(reason));
    obj.insert("hint".to_string(), json!(hint));
    obj.insert("path".to_string(), path_value);
    obj.insert("language".to_string(), language_value);
    if let Some(near) = case_near_match {
        obj.insert("case_near_match".to_string(), json!(near));
    }
    // Surface a hint when the caller's filter contained backslashes so the
    // slash-normalized interpretation is visible on Linux.
    if let Some(orig) = path
        && orig.contains('\\')
    {
        obj.insert("path_normalized_from".to_string(), json!(orig));
        if let Some(norm) = normalized_path {
            obj.insert("normalized_path".to_string(), json!(norm));
        }
    }
    Value::Object(obj)
}

fn graph_status_for_language(language: LanguageKind) -> &'static str {
    match language {
        LanguageKind::Unsupported => "unsupported_language",
        LanguageKind::Unknown => "unknown_language",
        _ => "indexed",
    }
}

// Trim policy: graph evidence packets drop `claim`, `freshness`, `provenance`,
// `cost_hint`, and `next_action` from the wire payload. The structured spans,
// symbol/edge bodies, and confidence id are the load-bearing data the model
// actually uses; the dropped fields were decorations that ate tokens without
// changing decisions. Telemetry that needs provenance/freshness reads from
// the typed graph events rather than the tool result JSON.
fn evidence_packet(
    _claim: impl Into<String>,
    _spans: Vec<Value>,
    _confidence: Confidence,
    _freshness: Freshness,
    _provenance: Vec<Provenance>,
    _cost_hint: ToolCostHint,
    _next_action: Value,
) -> Value {
    // The top-level `spans`/`confidence` mirror was an exact duplicate of the
    // data each caller already re-encodes in its body field (symbol/reference/
    // edge/hierarchy), so it is dropped here. The two callers that emit a bare
    // packet with no body field — `read_diff_packet` and the
    // `hierarchy_node_packet` fallback — add a minimal `spans`/`confidence`
    // back themselves so the model still has the path+span+confidence it needs.
    json!({})
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
    rank_label: Option<&str>,
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
        json!({}),
    );
    // The top-level `tool` mirror duplicates context the caller already knows
    // (the tool name is the call that produced this packet); the `symbol` body
    // identifies the symbol. Dropped to save tokens.
    let _ = tool;
    if let Some(object) = packet.as_object_mut() {
        let mut sym = symbol_json(graph, symbol);
        // Surface the rank tier when a query was provided so the model and TUI
        // users can inspect why one result beat another without reading source.
        if let (Some(label), Some(sym_obj)) = (rank_label, sym.as_object_mut()) {
            sym_obj.insert("rank".to_string(), json!(label));
        }
        object.insert("symbol".to_string(), sym);
    }
    packet
}

fn symbol_context_packet(
    graph: &squeezy_graph::SemanticGraph,
    symbol: &GraphSymbol,
    max_references: usize,
    sources: &mut SourceCache,
) -> Value {
    let mut packet = symbol_packet(graph, symbol, "symbol_context", None);
    if let Some(object) = packet.as_object_mut() {
        // Insert each collection only when non-empty: the common case (a symbol
        // with no callers/callees/references/diagnostics) used to ship four
        // empty arrays per packet, paying tokens for nothing.
        // Reuse a single shared `SourceCache` across every packet so a file
        // referenced by multiple result symbols is read from disk only once
        // per `symbol_context` call rather than once per symbol.
        let references = graph
            .references_to_symbol_with_cache(&symbol.id, sources)
            .into_iter()
            .take(max_references)
            .map(reference_json)
            .collect::<Vec<_>>();
        if !references.is_empty() {
            object.insert("references".to_string(), json!(references));
        }
        let callers = graph
            .callers(&symbol.id)
            .into_iter()
            .take(max_references)
            .filter_map(|hit| hit.caller)
            .map(|caller| symbol_summary_json(&caller))
            .collect::<Vec<_>>();
        if !callers.is_empty() {
            object.insert("callers".to_string(), json!(callers));
        }
        let callees = graph
            .callees(&symbol.id)
            .into_iter()
            .take(max_references)
            .filter_map(|hit| hit.callee)
            .map(|callee| symbol_summary_json(&callee))
            .collect::<Vec<_>>();
        if !callees.is_empty() {
            object.insert("callees".to_string(), json!(callees));
        }
        let mut diagnostics = graph
            .cargo_diagnostics_for_symbol(symbol)
            .into_iter()
            .take(max_references)
            .map(|hit| cargo_diagnostic_hit_json(&hit))
            .collect::<Vec<_>>();
        // Java/Dotnet/Kotlin build facts are diagnostics too: the cargo path
        // above only covers Rust. Append the language project facts for this
        // symbol's file (labelled `(label, detail)` pairs) so non-Rust build
        // signals surface in `symbol_context` instead of being Rust-only.
        for (label, detail) in graph.language_facts_for_file(&symbol.file_id) {
            diagnostics.push(json!({
                "level": "info",
                "label": label,
                "message": detail,
                "path": symbol.file_id.0,
                "source": "language_project_facts",
            }));
        }
        if !diagnostics.is_empty() {
            object.insert("diagnostics".to_string(), json!(diagnostics));
        }
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

/// Canonical lower-case label for a reference's syntactic kind, derived from the
/// `Debug` rendering of `ReferenceKind` (the same source `reference_json` uses
/// for the wire `kind`). Kept string-based so it needs no non-test dependency on
/// `squeezy-parse`'s `ReferenceKind` enum.
fn reference_kind_label(hit: &ReferenceHit) -> String {
    format!("{:?}", hit.reference.kind).to_ascii_lowercase()
}

/// True when a reference hit matches the requested `reference_kind` filter
/// (`identifier`|`type`|`path`|`field`|`attribute`, case-insensitive). An empty
/// or whitespace-only filter is treated as "no filter" and keeps every hit; an
/// unrecognized token simply matches nothing (the caller asked for a kind that
/// can't occur), which is the conservative, non-surprising behavior.
fn reference_kind_matches(hit: &ReferenceHit, filter: Option<&str>) -> bool {
    let Some(filter) = filter.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    reference_kind_label(hit) == filter.to_ascii_lowercase()
}

fn reference_matches_path(hit: &ReferenceHit, filter: &str) -> bool {
    // Use the same directory-aware filter `decl_search` uses so a `path=`
    // scope like `src/foo` matches references in `src/foo/bar.rs` (directory
    // boundary) while rejecting `src/foobar/baz.rs`. The previous
    // `path_matches_exact_or_suffix` only handled exact/trailing-segment
    // matches and silently ignored directory scopes.
    path_matches_filter(hit.reference.file_id.0.as_str(), filter)
}

/// Normalize a user-supplied path filter: replace backslashes with forward
/// slashes and strip leading `./` or `.\` so that Windows-style input like
/// `.\src\lib.rs` matches the indexed `src/lib.rs`.
fn normalize_path_filter(filter: &str) -> std::borrow::Cow<'_, str> {
    let s = if filter.contains('\\') {
        std::borrow::Cow::Owned(filter.replace('\\', "/"))
    } else {
        std::borrow::Cow::Borrowed(filter)
    };
    // Strip a leading `./` produced by shell tab-completion or model output.
    if let Some(rest) = s.strip_prefix("./") {
        std::borrow::Cow::Owned(rest.to_string())
    } else {
        s
    }
}

fn path_matches_exact_or_suffix(path: &str, filter: &str) -> bool {
    let filter = normalize_path_filter(filter);
    let filter = filter.as_ref();
    if path == filter {
        return true;
    }
    // Case-insensitive comparison on Windows where the filesystem ignores case.
    #[cfg(target_os = "windows")]
    if path.eq_ignore_ascii_case(filter) {
        return true;
    }
    if path
        .strip_suffix(filter)
        .is_some_and(|prefix| prefix.ends_with('/'))
    {
        return true;
    }
    // Case-insensitive suffix match on Windows.
    #[cfg(target_os = "windows")]
    {
        let path_lower = path.to_ascii_lowercase();
        let filter_lower = filter.to_ascii_lowercase();
        if path_lower
            .strip_suffix(filter_lower.as_str())
            .is_some_and(|prefix| prefix.ends_with('/'))
        {
            return true;
        }
    }
    false
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
    // The top-level `tool` mirror duplicates the call context; the `edge` body
    // already identifies the edge. Dropped to save tokens.
    let _ = tool;
    if let Some(object) = packet.as_object_mut() {
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
        .collect::<Vec<_>>();
    let mut chain_names = String::new();
    for symbol in &symbols {
        if !chain_names.is_empty() {
            chain_names.push_str(" -> ");
        }
        chain_names.push_str(&symbol.name);
    }
    let claim = format!("call chain found: {chain_names}");
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
            json!(
                symbols
                    .iter()
                    .map(|symbol| symbol_summary_json(symbol))
                    .collect::<Vec<_>>()
            ),
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
        return symbol_packet(graph, symbol, tool, None);
    }
    let mut packet = evidence_packet(
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
    );
    // Fallback bare packet (the node has no resolved symbol body), so keep a
    // minimal spans + confidence — this is the only path+span the model has
    // for an unresolved hierarchy node.
    if let Some(object) = packet.as_object_mut() {
        object.insert(
            "spans".to_string(),
            json!(vec![span_for_path_json(&node.name, Some(node.span))]),
        );
        object.insert(
            "confidence".to_string(),
            json!(Confidence::ExactSyntax.id()),
        );
    }
    packet
}

pub(crate) fn symbol_json(graph: &squeezy_graph::SemanticGraph, symbol: &GraphSymbol) -> Value {
    // Lean shape for first-emit symbol packets. Every dropped field paid for
    // itself in measured trace bytes without changing the model's next-call
    // routing:
    //   * `body_span` near-duplicated `span`; the model only needs it when
    //     it already decided to read the body, and at that point it calls
    //     `read_slice` with `symbol_id + span_kind=body` and the graph
    //     resolves the body span internally.
    //   * `attributes` is a search-time filter (`decl_search` accepts
    //     `attribute`), not a downstream decision input.
    //   * `language` and `freshness` were decorations the agent never
    //     branched on. Telemetry still carries both via typed graph events.
    //   * `visibility` and `dirty` are emitted only when set so the common
    //     case (unannotated symbols) sheds two keys per packet.
    let _ = graph;
    let mut object = serde_json::Map::with_capacity(8);
    object.insert("id".to_string(), json!(symbol.id.0));
    object.insert("name".to_string(), json!(symbol.name));
    object.insert("kind".to_string(), json!(format!("{:?}", symbol.kind)));
    object.insert("path".to_string(), json!(symbol.file_id.0));
    object.insert("signature".to_string(), json!(symbol.signature));
    object.insert("span".to_string(), span_json(symbol.span));
    if let Some(visibility) = symbol.visibility.as_deref() {
        object.insert("visibility".to_string(), json!(visibility));
    }
    if let Some(dirty) = symbol.dirty.as_ref() {
        object.insert(
            "dirty".to_string(),
            json!({
                "status": dirty.status,
                "ranges": dirty.ranges.iter().map(|range| json!({
                    "start_line": range.start_line,
                    "end_line": range.end_line,
                })).collect::<Vec<_>>(),
            }),
        );
    }
    object.insert("confidence".to_string(), json!(symbol.confidence.id()));
    Value::Object(object)
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
    // `freshness` and `provenance` were per-edge decorations the model never
    // branched on: freshness is "Fresh" in steady state and provenance is
    // the squeezy-graph stub. Telemetry still emits both via the typed
    // graph events; dropping them from the wire payload cuts ~40-60B per
    // edge and every call_graph/downstream_flow packet carries several.
    let mut value = json!({
        "from": edge.from.0,
        "to": edge.to.as_ref().map(|id| id.0.clone()),
        "target_text": edge.target_text,
        "kind": format!("{:?}", edge.kind),
        "span": edge.span.map(span_json),
        "confidence": edge.confidence.id(),
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
    // The nested `symbol` mirror (id, name, kind, path, span) duplicated every
    // field the node itself already carried except `path`, which we now hoist
    // directly. A real hierarchy result measured at ~24kB before this trim was
    // carrying ~10kB in nested symbol mirrors alone, and the redundant byte
    // coordinates inside `span_json` doubled the spend. `freshness` was a
    // per-node decoration the model never branched on (the file-level signal
    // still flows via the typed graph event), so dropping it shaves another
    // ~15B per node.
    let path = graph
        .symbols
        .get(&node.id)
        .map(|symbol| symbol.file_id.0.clone());
    json!({
        "id": node.id.0,
        "name": node.name,
        "kind": format!("{:?}", node.kind),
        "path": path,
        "span": span_json(node.span),
        "children": node.children.iter().map(|child| hierarchy_node_json(graph, child)).collect::<Vec<_>>(),
    })
}

/// Total number of nodes a `HierarchyNode` serializes to — itself plus every
/// descendant, since [`hierarchy_node_json`] recurses into `children`. Used to
/// size the `truncated` flag against what actually lands in the payload rather
/// than just the count of root nodes.
fn hierarchy_node_count(node: &HierarchyNode) -> usize {
    1 + node
        .children
        .iter()
        .map(hierarchy_node_count)
        .sum::<usize>()
}

#[allow(clippy::too_many_arguments)]
/// Filter a list of root [`HierarchyNode`]s by the test-scope of their
/// declaring file. Used by `hierarchy`'s `exclude_tests`/`tests_only`. Only the
/// root nodes are filtered (a non-test root may legitimately contain test
/// children and vice versa); children are kept intact under a surviving root.
fn filter_hierarchy_nodes_by_test_scope(
    graph: &squeezy_graph::SemanticGraph,
    nodes: Vec<HierarchyNode>,
    exclude_tests: bool,
    tests_only: bool,
) -> Vec<HierarchyNode> {
    if !exclude_tests && !tests_only {
        return nodes;
    }
    nodes
        .into_iter()
        .filter(|node| {
            let is_test = graph
                .symbols
                .get(&node.id)
                .map(symbol_is_test)
                .unwrap_or(false);
            passes_test_scope(is_test, exclude_tests, tests_only)
        })
        .collect()
}

/// Scope hierarchy nodes to a `result_path` subtree (see [`result_path_scope`]).
/// A node is kept when its declaring file path matches the filter; nodes with no
/// resolved symbol (and thus no path) are kept so an unanchored node is never
/// silently dropped. Returns the input unchanged when the filter is empty.
fn filter_hierarchy_nodes_by_path(
    graph: &squeezy_graph::SemanticGraph,
    nodes: Vec<HierarchyNode>,
    filter: Option<&str>,
) -> Vec<HierarchyNode> {
    let Some(filter) = filter else {
        return nodes;
    };
    nodes
        .into_iter()
        .filter(|node| match graph.symbols.get(&node.id) {
            Some(symbol) => path_matches_filter(symbol.file_id.0.as_str(), filter),
            None => true,
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn hierarchy_result(
    call: &ToolCall,
    manager: &GraphManager,
    refresh: &squeezy_graph::RefreshReport,
    graph: &squeezy_graph::SemanticGraph,
    nodes: Vec<HierarchyNode>,
    // Pre-cap count of roots. When `nodes` was already capped (e.g. via
    // `hierarchy_capped`) this is the real total so truncation is still
    // detectable; otherwise it equals `nodes.len()`.
    total_roots: usize,
    max_depth: usize,
    max_results: Option<usize>,
    offset: usize,
    root: Option<GraphSymbol>,
) -> ToolResult {
    let max_results = graph_limit(max_results);
    // Page the forest roots: skip `offset`, then keep `max_results`.
    let selected = nodes
        .iter()
        .skip(offset)
        .take(max_results)
        .collect::<Vec<_>>();
    // Count the total serialized nodes (roots + all recursively-emitted
    // children), not just the number of roots. A single wide root can blow past
    // `max_results` in the serialized `hierarchy`/`packets` while
    // `nodes.len() <= max_results`, which previously reported `truncated=false`.
    let serialized_nodes = selected
        .iter()
        .map(|node| hierarchy_node_count(node))
        .sum::<usize>();
    let truncated =
        total_roots.saturating_sub(offset) > max_results || serialized_nodes > max_results;
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
    payload.insert("offset".to_string(), json!(offset));
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
    // A live symbol_id wins; a stale one falls through to the query search.
    if let Some(symbol_id) = args.symbol_id.as_deref()
        && let Some(symbol) = graph.symbols.get(&SymbolId::new(symbol_id))
    {
        return Some(symbol.clone());
    }
    let query = args.query.as_deref()?;
    graph_symbol_search(
        graph,
        Some(query),
        args.kind.as_deref(),
        args.path.as_deref(),
        args.language.as_deref(),
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
    // A live target symbol_id wins; a stale one falls through to the query.
    if let Some(symbol_id) = args.target_symbol_id.as_deref()
        && let Some(symbol) = graph.symbols.get(&SymbolId::new(symbol_id))
    {
        return Some(symbol.clone());
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
        .map(|symbol| {
            let rank_label = args.query.as_deref().map(|q| symbol_rank_label(symbol, q));
            symbol_packet(graph, symbol, tool, rank_label)
        })
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
    // A live symbol_id wins; a stale one falls through to the query search.
    if let Some(symbol_id) = args.symbol_id.as_deref()
        && let Some(symbol) = graph.symbols.get(&SymbolId::new(symbol_id))
    {
        return Some(symbol.clone());
    }
    let query = args.query.as_deref()?;
    graph_symbol_search(
        graph,
        Some(query),
        args.kind.as_deref(),
        args.path.as_deref(),
        args.language.as_deref(),
        None,
        None,
    )
    .into_iter()
    .next()
}

/// Resolve a `path` filter to its workspace File symbol (kind `File`). Used by
/// the file-outline path of `hierarchy`: rooting the tree at the File symbol
/// yields that file's declaration tree instead of the whole-workspace forest.
///
/// Prefers a case-insensitive exact lookup (O(1), Windows-friendly) and falls
/// back to the directory-aware `path_matches_filter` scan so a partial but
/// unambiguous spelling still resolves. The File symbol is registered under the
/// stable `file:{file_id}` id, so the lookup needs no private graph accessor.
fn resolve_file_symbol(graph: &squeezy_graph::SemanticGraph, path: &str) -> Option<GraphSymbol> {
    if path.trim().is_empty() {
        return None;
    }
    let file = graph.find_file_case_insensitive(path).or_else(|| {
        graph
            .files
            .values()
            .find(|file| path_matches_filter(&file.relative_path, path))
    })?;
    let file_symbol_id = SymbolId::new(format!("file:{}", file.id.0));
    graph.symbols.get(&file_symbol_id).cloned()
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
        .map(|symbol| {
            let rank_label = if query.is_empty() {
                None
            } else {
                Some(symbol_rank_label(&symbol, query))
            };
            symbol_packet(graph, &symbol, "hierarchy", rank_label)
        })
        .collect::<Vec<_>>()
    };
    let mut payload = graph_payload("hierarchy", manager, refresh);
    payload.insert("resolved".to_string(), json!(false));
    payload.insert("symbol_id".to_string(), json!(args.symbol_id));
    payload.insert("query".to_string(), json!(args.query));
    payload.insert("packets".to_string(), json!(packets));
    // Mirror the flow path's dead-end: point the caller at definition_search to
    // resolve a unique symbol before re-asking, instead of leaving them with an
    // unactionable empty result.
    payload.insert(
        "next_action".to_string(),
        json!({
            "tool": "definition_search",
            "arguments": {"query": query},
            "reason": "resolve a unique symbol with definition_search"
        }),
    );
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

/// Returns the parsed [`ReadSliceArgs`] when `call` is a `read_slice` that
/// targets a plain `path` (no `symbol_id`) and therefore needs no graph.
/// Such reads pull bytes straight off disk, so they can — and should — run
/// even when the semantic graph is still indexing or structurally
/// unavailable (bug #1). Returns `None` for any other tool, for a
/// `symbol_id`-based read_slice (which still requires the graph), or when the
/// arguments fail to deserialize (so the normal dispatch path surfaces the
/// arg error).
fn read_slice_path_only_args(call: &ToolCall) -> Option<ReadSliceArgs> {
    if call.name != "read_slice" {
        return None;
    }
    let args = serde_json::from_value::<ReadSliceArgs>(call.arguments.clone()).ok()?;
    if args.symbol_id.is_some() {
        return None;
    }
    Some(args)
}

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
            .ok_or_else(|| {
                // A stale id (graph re-indexed since it was minted) is recoverable:
                // tell the caller to re-resolve by name rather than reporting an
                // opaque "not found".
                format!("symbol_id stale; re-resolve via definition_search (stale id: {symbol_id})")
            })?;
        let span = match args.span_kind.unwrap_or_default() {
            // Read only the declaration header when the extractor pinned a real
            // signature span; fall back to the full node for bodyless symbols
            // and heuristic extractors that left it `None`.
            ReadSliceSpanKind::Signature => symbol.signature_span.unwrap_or(symbol.span),
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
    // Find graph status: try exact/suffix first, then case-insensitive fallback
    // so a Windows user typing `SRC\lib.rs` gets the correct indexed language
    // rather than "not_indexed".
    let status = graph
        .and_then(|graph| {
            graph
                .files
                .values()
                // Use the directory-aware, Windows-normalised predicate so
                // that directory-shaped paths and backslash-escaped inputs
                // resolve consistently with the symbol-search predicates.
                .find(|file| path_matches_filter(&file.relative_path, &path))
                .or_else(|| graph.find_file_case_insensitive(&path))
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

/// Prefix each line of `content` with its 1-based absolute line number,
/// followed by a tab. Earlier this used cat -n's 6-char right-aligned
/// format (`     1\t<line>`), which lined up nicely visually but cost
/// 7 bytes/line of overhead regardless of how many digits the line
/// number actually had. Per-trace cost analysis on mini-ruby showed
/// the prefix accounted for 20–25 % of read_file payload bytes; the
/// model never branches on the padding, only the line number itself.
/// Compact form (`1\t<line>`) gives the same parsing affordance at
/// 2–3 bytes/line for typical files. `start_line` carries the first
/// line's absolute number on the result envelope so the model still
/// has the anchor without counting newlines.
pub(crate) fn prefix_lines_with_numbers(content: &str, start_line: u32) -> String {
    if content.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(content.len() + content.len() / 12);
    let mut line_no = start_line;
    for piece in content.split_inclusive('\n') {
        use std::fmt::Write as _;
        let _ = write!(out, "{line_no}\t{piece}");
        line_no = line_no.saturating_add(1);
    }
    out
}

/// Inverse of [`prefix_lines_with_numbers`]: strip the leading `"{digits}\t"`
/// gutter from each line so stored, line-numbered `read_slice` output can be
/// diffed against raw file bytes. A line without the expected gutter (e.g.
/// content that was never line-numbered) is passed through unchanged so this
/// never corrupts non-prefixed input.
fn strip_line_number_prefixes(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    for piece in content.split_inclusive('\n') {
        // `split_inclusive` keeps the trailing `\n`, so the gutter we strip is
        // the `{digits}\t` at the very start of `piece`. Only strip when the
        // prefix before the first tab is all ASCII digits — otherwise the line
        // is not a numbered render and must survive verbatim.
        let stripped = match piece.split_once('\t') {
            Some((number, rest))
                if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) =>
            {
                rest
            }
            _ => piece,
        };
        out.push_str(stripped);
    }
    out
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

/// Targeted total span (in lines) the read_slice line-range mode auto-widens
/// toward when the caller passes a tight window. Sized to cover a typical
/// function/method body plus a few lines of surrounding context in one fetch.
/// The caller's requested `[start_line, end_line]` is always fully included,
/// so this padding only ever adds context — it can never drop a requested
/// line, and recall cannot regress if it's tight. Kept modest because
/// read_slice is the single highest-volume tool and most callers request a
/// far narrower window (Haiku's median line read is ~20 lines); over-padding
/// every such read toward a large target inflates input tokens on the
/// dominant cost driver for no recall benefit.
const READ_SLICE_AUTO_WIDEN_TARGET_LINES: u32 = 48;
/// Threshold below which a caller-supplied line range is treated as "too
/// tight" and gets auto-widened up to `READ_SLICE_AUTO_WIDEN_TARGET_LINES`.
/// Ranges already at or above this size are left exactly as the caller asked
/// — a caller that deliberately requested a wide window already has its
/// context and should not be padded further.
const READ_SLICE_AUTO_WIDEN_THRESHOLD_LINES: u32 = 40;

fn line_window(
    text: &str,
    args: &ReadSliceArgs,
) -> std::result::Result<(usize, usize, SourceSpan), String> {
    let total_lines = text.lines().count().max(1) as u32;
    let context = args.context_lines.unwrap_or(0);
    let raw_start = args.start_line.unwrap_or(1).max(1);
    let raw_end = args
        .end_line
        .unwrap_or(args.start_line.unwrap_or(total_lines))
        .max(raw_start);
    // Auto-widen tight caller-supplied windows so the enclosing
    // function/impl block fits in one fetch. Honored only when `start_line`
    // (and optionally `end_line`) drove the call — symbol_id / byte modes
    // never reach this helper, and explicit `context_lines` still adds on top
    // of the widened range.
    let (auto_pad_above, auto_pad_below) = if args.start_line.is_some() {
        let requested = raw_end.saturating_sub(raw_start).saturating_add(1);
        if requested < READ_SLICE_AUTO_WIDEN_THRESHOLD_LINES {
            let extra = READ_SLICE_AUTO_WIDEN_TARGET_LINES.saturating_sub(requested);
            let above = extra / 2;
            let below = extra - above;
            (above, below)
        } else {
            (0, 0)
        }
    } else {
        (0, 0)
    };
    let start_line = raw_start
        .saturating_sub(auto_pad_above)
        .saturating_sub(context)
        .max(1);
    let end_line = raw_end
        .saturating_add(auto_pad_below)
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
    // The wire payload addresses spans by 1-based line numbers only. The
    // model routes every follow-up by line range (`read_slice` accepts
    // `start_line`/`end_line`, and the agent compaction pipeline derives
    // the byte window from `offset` + `content.len()` rather than reading
    // a span's byte offsets), so the raw byte coordinates were doubling
    // each span's footprint without changing decisions. `end.column` is
    // dropped for the same reason — start column plus the line range pins
    // the span. Saves ~40-60B per span across every packet, repo_map node,
    // symbol, edge, and reference hit.
    // `start.column` is omitted when it is 0 (the overwhelmingly common case
    // for declaration spans, which start at column 0): the line range alone
    // pins the span and the model routes follow-ups by line, so a zero column
    // is pure overhead.
    let mut start = serde_json::Map::with_capacity(2);
    start.insert("line".to_string(), json!(span.start.line));
    if span.start.column != 0 {
        start.insert("column".to_string(), json!(span.start.column));
    }
    json!({
        "start": Value::Object(start),
        "end": {"line": span.end.line},
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
        // Bug #1: a path-only `read_slice` never consults the graph, so don't
        // make it wait on background indexing — only block on graph readiness
        // for calls that actually need the graph. If the graph happens to be
        // ready already we still fall through and pass it down for richer
        // `graph_status` classification; this only skips the *wait*.
        let path_only_read_slice = read_slice_path_only_args(call).is_some();
        let graph_ready = if path_only_read_slice {
            self.wait_for_graph_ready(Duration::ZERO)
        } else {
            self.wait_for_graph_ready(graph_ready_wait())
        };
        // A poisoned mutex used to brick every graph tool. Recover the guard
        // instead so a panic in one handler doesn't permanently disable the
        // rest of the semantic toolset.
        let mut graph = self.graph.lock().unwrap_or_else(|e| e.into_inner());
        let Some(manager) = graph.as_mut() else {
            // Bug #1: a path-only `read_slice` (no `symbol_id`) reads bytes
            // straight off disk and never touches the graph. Don't strand it on
            // a `graph_unavailable` result just because the graph is still
            // indexing or structurally absent — route it through the path-read
            // path with `graph=None`. `symbol_id`-based read_slice still needs
            // the graph and falls through to the unavailable result below.
            // (A wait was already skipped above when the graph isn't ready, so
            // this also avoids stalling the read behind background indexing.)
            if let Some(args) = read_slice_path_only_args(call) {
                // Release the graph lock before the (potentially slow) file read.
                drop(graph);
                return self.execute_read_slice_blocking(call, args, None);
            }
            // Surface the open error (if any) so the model can distinguish a
            // store/parse failure from a workspace that genuinely has no graph.
            let open_error = self.graph_open_error();
            return graph_unavailable_result_with_error(call, !graph_ready, open_error);
        };
        let refresh = match manager.refresh_before_query() {
            Ok(report) => report,
            Err(err) => return tool_error(call, err),
        };
        crate::emit_graph_refresh_telemetry(&self.telemetry, &refresh);
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
            "inheritance_hierarchy" => {
                match serde_json::from_value::<InheritanceHierarchyArgs>(call.arguments.clone()) {
                    Ok(args) => {
                        self.execute_inheritance_hierarchy_blocking(call, args, manager, &refresh)
                    }
                    Err(err) => tool_arg_error(call, err),
                }
            }
            "impact" => match serde_json::from_value::<ImpactArgs>(call.arguments.clone()) {
                Ok(args) => self.execute_impact_blocking(call, args, manager, &refresh),
                Err(err) => tool_arg_error(call, err),
            },
            "symbol_at" => match serde_json::from_value::<SymbolAtArgs>(call.arguments.clone()) {
                Ok(args) => self.execute_symbol_at_blocking(call, args, manager, &refresh),
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
        let path_filter = args.path.as_deref().filter(|p| !p.trim().is_empty());
        let language_filter = args.language.as_deref().filter(|l| !l.trim().is_empty());
        // Cap the roots before expansion instead of building the entire forest
        // and discarding all but `max_files`; `total_roots` is the pre-cap count
        // used for the truncation signal.
        //
        // When a path/language scope is requested the cap can't run before the
        // filter (it would cap on unscoped roots and under-return), so build the
        // full forest, filter the roots by the declaring file's path/language,
        // then cap. Unscoped repo_map keeps the cheap pre-capped fast path.
        let (nodes, total_roots) = if path_filter.is_some() || language_filter.is_some() {
            let all = graph.hierarchy(None, max_depth);
            let filtered: Vec<HierarchyNode> = all
                .into_iter()
                .filter(|node| {
                    let Some(symbol) = graph.symbols.get(&node.id) else {
                        return false;
                    };
                    path_filter
                        .map(|p| path_matches_filter(symbol.file_id.0.as_str(), p))
                        .unwrap_or(true)
                        && file_language_matches(graph, &symbol.file_id, language_filter)
                })
                .collect();
            let total = filtered.len();
            let capped = filtered.into_iter().take(max_files).collect::<Vec<_>>();
            (capped, total)
        } else {
            graph.hierarchy_capped(None, max_depth, max_files)
        };
        let selected = nodes.iter().collect::<Vec<_>>();
        // Count total serialized nodes (roots + recursively-emitted children),
        // not just the number of roots, so a single wide root that exceeds
        // `max_files` in the serialized output is reported as truncated.
        let serialized_nodes = selected
            .iter()
            .map(|node| hierarchy_node_count(node))
            .sum::<usize>();
        let truncated = total_roots > max_files || serialized_nodes > max_files;
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
        // Coverage counts honor the same path/language scope as the hierarchy so
        // a scoped `repo_map` reports the languages present in that subtree, not
        // the whole workspace. The `stats` block stays whole-graph (its cargo /
        // index aggregates are not file-scopable) and is tagged accordingly.
        if path_filter.is_some() || language_filter.is_some() {
            payload.insert(
                "languages".to_string(),
                graph_language_counts_scoped_json(graph, path_filter, language_filter),
            );
            payload.insert("scoped".to_string(), json!(true));
            payload.insert(
                "stats_scope".to_string(),
                json!("whole_graph; languages restricted to path/language scope"),
            );
        } else {
            payload.insert("languages".to_string(), graph_language_counts_json(graph));
        }
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
        // Transitive subtype closure: when the caller asks for `transitive=true`
        // AND the attribute names a supertype (`base:`/`mixin:`/`iface:`), walk
        // the whole subtype tree by name instead of returning only the direct
        // subtypes the single-pass attribute filter surfaces. Any other shape
        // (no transitive flag, or a non-inheritance attribute) is unchanged.
        let transitive_seed = args.transitive.unwrap_or(false).then(|| {
            args.attribute
                .as_deref()
                .filter(|attr| attribute_has_inheritance_prefix(attr))
                .map(seed_type_names)
        });
        // `kind` is multi-valued: "struct|enum|trait" matches any of the listed
        // kinds. Each pass reuses the existing single-kind search and the union
        // dedups by id; a single-token / absent kind runs exactly one pass.
        let (symbols, closure_capped) = match transitive_seed {
            Some(Some(seed_names)) if !seed_names.is_empty() => {
                let mut capped = false;
                let symbols = multi_kind_symbol_union(args.kind.as_deref(), |kind| {
                    let (symbols, pass_capped) = graph_transitive_subtype_closure(
                        graph,
                        args.query.as_deref(),
                        kind,
                        args.path.as_deref(),
                        args.language.as_deref(),
                        args.visibility.as_deref(),
                        &seed_names,
                        TRANSITIVE_CLOSURE_CAP,
                    );
                    capped |= pass_capped;
                    symbols
                });
                (symbols, capped)
            }
            _ => (
                multi_kind_symbol_union(args.kind.as_deref(), |kind| {
                    graph_symbol_search(
                        graph,
                        args.query.as_deref(),
                        kind,
                        args.path.as_deref(),
                        args.language.as_deref(),
                        args.visibility.as_deref(),
                        args.attribute.as_deref(),
                    )
                }),
                false,
            ),
        };
        // Dead-code mode: drop any scanned declaration that has more than
        // `max_callers` (default 0) inbound usage edges. Unscanned stubs
        // (external/parse-failed) are dropped outright — they are not real
        // declarations in this workspace and would be false positives.
        let unused = args.unused.unwrap_or(false);
        let max_callers = args.max_callers.unwrap_or(0);
        let exclude_tests = args.exclude_tests.unwrap_or(false);
        let tests_only = args.tests_only.unwrap_or(false);
        let confidence_scope = ConfidenceScope::new(
            args.confidence.as_deref(),
            args.exclude_external.unwrap_or(false),
            args.external_only.unwrap_or(false),
        );
        let symbols: Vec<GraphSymbol> = symbols
            .into_iter()
            .filter(|symbol| passes_test_scope(symbol_is_test(symbol), exclude_tests, tests_only))
            .filter(|symbol| confidence_scope.keeps(symbol.confidence))
            .filter(|symbol| !unused || symbol.scanned)
            .filter(|symbol| !unused || inbound_usage_count(graph, symbol) <= max_callers)
            .collect();
        // The closure cap is a distinct truncation source from the result-window
        // cap: fold it into `truncated` so the page is flagged, and also surface
        // it as a separate `closure_capped` signal so a caller can tell the
        // subtype *tree* itself was clipped (and may re-scope the walk).
        let truncated = closure_capped || symbols.len().saturating_sub(offset) > max_results;
        let selected = symbols
            .iter()
            .skip(offset)
            .take(max_results)
            .cloned()
            .collect::<Vec<_>>();
        let packets = selected
            .iter()
            .map(|symbol| {
                let rank_label = args.query.as_deref().map(|q| symbol_rank_label(symbol, q));
                let mut packet = symbol_packet(graph, symbol, "decl_search", rank_label);
                // In dead-code mode, annotate each surviving candidate so the
                // model never treats a flagged/caveated result as confidently
                // dead: `exported` (may have callers outside this graph) and
                // `caveat` (only low-confidence edges, so absence of resolved
                // uses is not proof). A clean candidate gets `confidently dead`.
                if unused
                    && let Some(object) = packet.as_object_mut()
                {
                    let exported = visibility_is_exported(symbol);
                    let caveat = confidence_is_unresolved(symbol.confidence);
                    let mut flags = serde_json::Map::new();
                    flags.insert("exported".to_string(), json!(exported));
                    if caveat {
                        flags.insert(
                            "caveat".to_string(),
                            json!("no resolved references found; this declaration's edges resolved at low confidence (macro/conditional/external/candidate), so absence of uses is not proof of dead code"),
                        );
                    }
                    if exported {
                        flags.insert(
                            "note".to_string(),
                            json!("public/exported: may have callers outside the scanned workspace"),
                        );
                    }
                    flags.insert(
                        "verdict".to_string(),
                        json!(if exported || caveat {
                            "possibly_dead"
                        } else {
                            "likely_dead"
                        }),
                    );
                    object.insert("dead_code".to_string(), Value::Object(flags));
                }
                packet
            })
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
                None,
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
        payload.insert("closure_capped".to_string(), json!(closure_capped));
        if unused {
            payload.insert("unused".to_string(), json!(true));
            payload.insert("max_callers".to_string(), json!(max_callers));
        }
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
        // `kind` is multi-valued ("struct|enum|trait"): resolve candidates once
        // per kind token and union by id, reusing the single-kind resolver.
        let mut symbols = multi_kind_symbol_union(args.kind.as_deref(), |kind| {
            resolve_definition_candidates(
                graph,
                args.symbol_id.as_deref(),
                args.query.as_deref(),
                kind,
                args.path.as_deref(),
                args.language.as_deref(),
            )
        });
        let confidence_scope = ConfidenceScope::new(
            args.confidence.as_deref(),
            args.exclude_external.unwrap_or(false),
            args.external_only.unwrap_or(false),
        );
        if !confidence_scope.is_noop() {
            symbols.retain(|symbol| confidence_scope.keeps(symbol.confidence));
        }
        let candidate_count = symbols.len();
        // Page after sorting: skip `offset`, then keep `max_results`. `truncated`
        // reflects whether anything remains past the returned window.
        let offset = args.offset.unwrap_or(0);
        let truncated = candidate_count.saturating_sub(offset) > max_results;
        let selected = symbols
            .into_iter()
            .skip(offset)
            .take(max_results)
            .collect::<Vec<_>>();
        let packets = selected
            .iter()
            .map(|symbol| {
                let rank_label = args.query.as_deref().map(|q| symbol_rank_label(symbol, q));
                symbol_packet(graph, symbol, "definition_search", rank_label)
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
                args.symbol_id.as_deref(),
                args.path.as_deref(),
                args.query.as_deref(),
                packet_count,
            ),
        );
        payload.insert("offset".to_string(), json!(offset));
        payload.insert("truncated".to_string(), json!(truncated));
        payload.insert("total_candidates".to_string(), json!(candidate_count));
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
        let exclude_tests = args.exclude_tests.unwrap_or(false);
        let tests_only = args.tests_only.unwrap_or(false);
        let confidence_scope = ConfidenceScope::new(
            args.confidence.as_deref(),
            args.exclude_external.unwrap_or(false),
            args.external_only.unwrap_or(false),
        );
        let filtered = hits
            .into_iter()
            .filter(|hit| {
                args.path
                    .as_deref()
                    .map(|path| reference_matches_path(hit, path))
                    .unwrap_or(true)
            })
            .filter(|hit| {
                passes_test_scope(
                    path_is_test(hit.reference.file_id.0.as_str()),
                    exclude_tests,
                    tests_only,
                )
            })
            .filter(|hit| {
                file_language_matches(graph, &hit.reference.file_id, args.language.as_deref())
            })
            .filter(|hit| reference_kind_matches(hit, args.reference_kind.as_deref()))
            .filter(|hit| confidence_scope.keeps(hit.confidence))
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
                args.symbol_id.as_deref(),
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
        let exclude_tests = args.exclude_tests.unwrap_or(false);
        let tests_only = args.tests_only.unwrap_or(false);
        if exclude_tests || tests_only {
            packets.retain(|packet| packet_matches_test_scope(packet, exclude_tests, tests_only));
        }
        // edge_kind: keep only edge-bearing packets whose kind matches the
        // requested kind (case-insensitive). An unknown token parses to `None`
        // and leaves every packet in place.
        if let Some(want) = args.edge_kind.as_deref().and_then(parse_edge_kind_filter) {
            packets.retain(|packet| packet_matches_edge_kind(packet, Some(want)));
        }
        // path/result_path scopes the RESULT packets, not just the root: drop any
        // caller/edge packet whose file path falls outside the requested subtree.
        let result_path = result_path_scope(args.path.as_deref(), args.result_path.as_deref());
        if result_path.is_some() {
            packets.retain(|packet| packet_matches_result_path(packet, result_path));
        }
        // Confidence scope: keep only packets at an allowed confidence, and/or
        // drop / isolate out-of-workspace (`External`) targets.
        let confidence_scope = ConfidenceScope::new(
            args.confidence.as_deref(),
            args.exclude_external.unwrap_or(false),
            args.external_only.unwrap_or(false),
        );
        if !confidence_scope.is_noop() {
            packets.retain(|packet| confidence_scope.keeps_packet(packet));
        }
        // Pagination over the filtered packets: skip `offset`, keep `max_results`.
        // The page can be truncated either because the BFS overflowed upstream or
        // because filtered packets remain past the returned window.
        let offset = args.offset.unwrap_or(0);
        let filtered_total = packets.len();
        let truncated = overflowed || filtered_total.saturating_sub(offset) > max_results;
        let packets: Vec<Value> = packets.into_iter().skip(offset).take(max_results).collect();
        let confidence_distribution = ToolCostHint::confidence_distribution_from_packets(&packets);
        let mut payload = graph_payload("upstream_flow", manager, refresh);
        payload.insert("symbol".to_string(), symbol_json(graph, &symbol));
        payload.insert("max_depth".to_string(), json!(max_depth));
        payload.insert("offset".to_string(), json!(offset));
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
            // Use the per-source edge index instead of scanning every edge in
            // the graph; the kind filter stays the same.
            let outgoing = graph
                .outgoing_edges(&symbol.id)
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
        let exclude_tests = args.exclude_tests.unwrap_or(false);
        let tests_only = args.tests_only.unwrap_or(false);
        if exclude_tests || tests_only {
            packets.retain(|packet| packet_matches_test_scope(packet, exclude_tests, tests_only));
        }
        // edge_kind: keep only edge-bearing packets whose kind matches the
        // requested kind (case-insensitive). An unknown token parses to `None`
        // and leaves every packet in place.
        if let Some(want) = args.edge_kind.as_deref().and_then(parse_edge_kind_filter) {
            packets.retain(|packet| packet_matches_edge_kind(packet, Some(want)));
        }
        // path/result_path scopes the RESULT packets, not just the root: drop any
        // callee/edge packet whose file path falls outside the requested subtree.
        let result_path = result_path_scope(args.path.as_deref(), args.result_path.as_deref());
        if result_path.is_some() {
            packets.retain(|packet| packet_matches_result_path(packet, result_path));
        }
        // Confidence scope: keep only packets at an allowed confidence, and/or
        // drop / isolate out-of-workspace (`External`) targets.
        let confidence_scope = ConfidenceScope::new(
            args.confidence.as_deref(),
            args.exclude_external.unwrap_or(false),
            args.external_only.unwrap_or(false),
        );
        if !confidence_scope.is_noop() {
            packets.retain(|packet| confidence_scope.keeps_packet(packet));
        }
        // Pagination over the filtered packets: skip `offset`, keep `max_results`.
        // The page can be truncated either because the BFS overflowed downstream
        // or because filtered packets remain past the returned window.
        let offset = args.offset.unwrap_or(0);
        let filtered_total = packets.len();
        let truncated = overflowed || filtered_total.saturating_sub(offset) > max_results;
        let packets: Vec<Value> = packets.into_iter().skip(offset).take(max_results).collect();
        let confidence_distribution = ToolCostHint::confidence_distribution_from_packets(&packets);
        let mut payload = graph_payload("downstream_flow", manager, refresh);
        payload.insert("symbol".to_string(), symbol_json(graph, &symbol));
        payload.insert("max_depth".to_string(), json!(max_depth));
        payload.insert("offset".to_string(), json!(offset));
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
        let language_filter = args.language.as_deref();
        let diff_only = args.diff_only.unwrap_or(false);
        let exclude_tests = args.exclude_tests.unwrap_or(false);
        let tests_only = args.tests_only.unwrap_or(false);
        let confidence_scope = ConfidenceScope::new(
            args.confidence.as_deref(),
            args.exclude_external.unwrap_or(false),
            args.external_only.unwrap_or(false),
        );
        // A live symbol_id from a sibling tool resolves the target directly;
        // a stale id (graph re-indexed since it was minted) falls through to
        // the name query so the call still lands on the right symbol.
        let resolved_by_id = args
            .symbol_id
            .as_deref()
            .and_then(|id| graph.symbols.get(&SymbolId::new(id)).cloned());
        // Count candidates BEFORE `take(max_results)` so the response can
        // report `truncated` honestly: emitting `truncated:false` while
        // dropping matches misleads the model into thinking it saw everything.
        let candidates = match resolved_by_id {
            Some(symbol) => vec![symbol],
            None => graph_symbol_search(
                graph,
                Some(&args.query),
                None,
                path_filter,
                language_filter,
                None,
                None,
            ),
        }
        .into_iter()
        .filter(|symbol| {
            !diff_only || symbol.dirty.is_some() || dirty_paths.contains(&symbol.file_id.0)
        })
        .filter(|symbol| language_matches(graph, symbol, language_filter))
        .filter(|symbol| passes_test_scope(symbol_is_test(symbol), exclude_tests, tests_only))
        .filter(|symbol| confidence_scope.keeps(symbol.confidence))
        .collect::<Vec<_>>();
        // Page after filtering/sorting: skip `offset`, keep `max_results`.
        let offset = args.offset.unwrap_or(0);
        let mut pre_take_len = candidates.len();
        let mut symbols = candidates
            .into_iter()
            .skip(offset)
            .take(max_results)
            .collect::<Vec<_>>();
        if symbols.is_empty() && diff_only {
            let fallback = graph
                .dirty_symbols()
                .into_iter()
                .filter(|symbol| symbol_matches_path_filter(symbol, path_filter))
                .filter(|symbol| language_matches(graph, symbol, language_filter))
                .filter(|symbol| {
                    passes_test_scope(symbol_is_test(symbol), exclude_tests, tests_only)
                })
                .filter(|symbol| confidence_scope.keeps(symbol.confidence))
                .filter(|symbol| {
                    symbol.name.contains(&args.query) || symbol.signature.contains(&args.query)
                })
                .collect::<Vec<_>>();
            pre_take_len = fallback.len();
            symbols = fallback
                .into_iter()
                .skip(offset)
                .take(max_results)
                .collect();
        }
        let truncated = pre_take_len.saturating_sub(offset) > max_results;
        // One cache shared across every result packet: a file referenced by
        // several of the returned symbols is read from disk once, not per symbol.
        let mut sources = SourceCache::default();
        let packets = symbols
            .iter()
            .map(|symbol| symbol_context_packet(graph, symbol, max_references, &mut sources))
            .collect::<Vec<_>>();
        let confidence_distribution =
            ToolCostHint::confidence_distribution_from(symbols.iter().map(|s| s.confidence));
        let mut payload = graph_payload("symbol_context", manager, refresh);
        payload.insert("query".to_string(), json!(args.query));
        payload.insert("symbol_id".to_string(), json!(args.symbol_id));
        payload.insert(
            "mode".to_string(),
            json!(diff_mode_str(args.mode.unwrap_or_default())),
        );
        payload.insert("diff_only".to_string(), json!(diff_only));
        payload.insert("offset".to_string(), json!(offset));
        let packet_count = packets.len();
        payload.insert("packets".to_string(), json!(packets));
        payload.insert(
            "fallback".to_string(),
            graph_zero_hit_fallback(
                graph,
                args.symbol_id.as_deref(),
                path_filter,
                Some(&args.query),
                packet_count,
            ),
        );
        payload.insert("truncated".to_string(), json!(truncated));
        make_result(
            call,
            ToolStatus::Success,
            Value::Object(payload),
            ToolCostHint {
                matches_returned: symbols.len() as u64,
                truncated,
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
        let offset = args.offset.unwrap_or(0);
        // Result-packet scope: `result_path` (or the plain `path`) restricts the
        // emitted nodes to a subtree, not just the root resolution.
        let result_path = result_path_scope(args.path.as_deref(), args.result_path.as_deref());
        let root = resolve_hierarchy_root(graph, &args);
        if args.symbol_id.is_some() || args.query.is_some() {
            let Some(root) = root else {
                return unresolved_hierarchy_result(call, manager, refresh, &args);
            };
            let nodes = graph.hierarchy(Some(&root.id), max_depth);
            let nodes = filter_hierarchy_nodes_by_path(graph, nodes, result_path);
            let total_roots = nodes.len();
            return hierarchy_result(
                call,
                manager,
                refresh,
                graph,
                nodes,
                total_roots,
                max_depth,
                args.max_results,
                offset,
                Some(root),
            );
        }
        // File outline: a `path` with no symbol_id/query roots the hierarchy at
        // the file's File symbol so the result is that file's declaration tree
        // (functions/classes/etc.), not the whole-workspace rootless forest.
        if let Some(path) = args.path.as_deref()
            && let Some(file_sym) = resolve_file_symbol(graph, path)
        {
            let nodes = graph.hierarchy(Some(&file_sym.id), max_depth);
            // The file is already the path scope; an explicit `result_path` can
            // narrow further within that file's declaration tree.
            let nodes = filter_hierarchy_nodes_by_path(graph, nodes, args.result_path.as_deref());
            let total_roots = nodes.len();
            return hierarchy_result(
                call,
                manager,
                refresh,
                graph,
                nodes,
                total_roots,
                max_depth,
                args.max_results,
                offset,
                Some(file_sym),
            );
        }
        // Rootless map: cap the forest roots before expansion instead of
        // building the whole forest and discarding all but `max_results`. The cap
        // must include `offset` so the requested page is still reachable.
        let max_results = graph_limit(args.max_results);
        let cap = offset.saturating_add(max_results);
        let (nodes, total_roots) = graph.hierarchy_capped(None, max_depth, cap);
        let exclude_tests = args.exclude_tests.unwrap_or(false);
        let tests_only = args.tests_only.unwrap_or(false);
        let nodes = filter_hierarchy_nodes_by_test_scope(graph, nodes, exclude_tests, tests_only);
        let path_scoped = result_path.is_some();
        let nodes = filter_hierarchy_nodes_by_path(graph, nodes, result_path);
        // `total_roots` came from the pre-filter cap; recompute against the
        // surviving roots so `truncated` reflects what the caller can actually
        // page through under the test/path scope.
        let total_roots = if exclude_tests || tests_only || path_scoped {
            nodes.len()
        } else {
            total_roots
        };
        hierarchy_result(
            call,
            manager,
            refresh,
            graph,
            nodes,
            total_roots,
            max_depth,
            args.max_results,
            offset,
            None,
        )
    }

    fn execute_inheritance_hierarchy_blocking(
        &self,
        call: &ToolCall,
        args: InheritanceHierarchyArgs,
        manager: &GraphManager,
        refresh: &squeezy_graph::RefreshReport,
    ) -> ToolResult {
        let graph = manager.graph();
        let max_results = graph_limit(args.max_results);
        let subtypes = args.subtypes.unwrap_or(false);

        // Resolve the root symbol via id or text query. A live symbol_id wins;
        // a stale one (graph re-indexed) transparently falls through to the
        // query so the caller isn't dead-ended on an empty result.
        let root = if let Some(id) = args.symbol_id.as_deref()
            && let Some(sym) = graph.symbols.get(&SymbolId::new(id))
        {
            Some(sym.clone())
        } else if let Some(q) = args.query.as_deref() {
            graph_symbol_search(
                graph,
                Some(q),
                None,
                None,
                args.language.as_deref(),
                None,
                None,
            )
            .into_iter()
            .next()
        } else {
            None
        };

        let Some(root_sym) = root else {
            // Dead-end: surface candidate packets and a re-resolve next_action so
            // the caller can pick a unique symbol, mirroring the flow/hierarchy
            // unresolved paths instead of returning a bare error.
            let query = args.query.as_deref().unwrap_or("");
            let candidate_packets = if query.is_empty() {
                Vec::new()
            } else {
                graph_symbol_search(graph, Some(query), None, None, None, None, None)
                    .into_iter()
                    .take(DEFAULT_GRAPH_MAX_RESULTS)
                    .map(|symbol| {
                        let rank_label = symbol_rank_label(&symbol, query);
                        symbol_packet(graph, &symbol, "inheritance_hierarchy", Some(rank_label))
                    })
                    .collect::<Vec<_>>()
            };
            let mut payload = graph_payload("inheritance_hierarchy", manager, refresh);
            payload.insert("error".to_string(), json!("symbol not found"));
            payload.insert("resolved".to_string(), json!(false));
            payload.insert("symbol_id".to_string(), json!(args.symbol_id));
            payload.insert("query".to_string(), json!(args.query));
            payload.insert("packets".to_string(), json!(candidate_packets));
            payload.insert(
                "next_action".to_string(),
                json!({
                    "tool": "definition_search",
                    "arguments": {"query": query},
                    "reason": "resolve a unique symbol with definition_search"
                }),
            );
            return make_result(
                call,
                ToolStatus::Stale,
                Value::Object(payload),
                ToolCostHint::default(),
                None,
            );
        };

        // Member mode: when the root is a Method/Field and `member=true`, return
        // the overrides/implementations of that member across subtypes instead
        // of the type-to-type walk. A non-member root falls back to the type
        // walk so the flag never dead-ends a mistargeted call.
        let member_requested = args.member.unwrap_or(false);
        let member_mode =
            member_requested && matches!(root_sym.kind, SymbolKind::Method | SymbolKind::Field);
        // Transitive subtype closure: only meaningful in subtype mode (ancestors
        // are already a full transitive walk; member mode resolves overrides
        // directly). `max_depth` is clamped to the graph's traversal bounds; the
        // default matches the other depth-bounded graph walks.
        let transitive_subtypes = subtypes && !member_mode && args.transitive.unwrap_or(false);
        let subtype_depth = args
            .max_depth
            .unwrap_or(DEFAULT_GRAPH_MAX_DEPTH)
            .clamp(1, MAX_GRAPH_MAX_DEPTH);
        let related: Vec<GraphSymbol> = if member_mode {
            graph.member_implementations(&root_sym.id)
        } else if subtypes {
            if transitive_subtypes {
                graph.inheritance_subtypes_transitive(&root_sym.id, subtype_depth)
            } else {
                graph.inheritance_direct_subtypes(&root_sym.id)
            }
        } else {
            graph.inheritance_ancestors(&root_sym.id)
        };
        let language_filter = args.language.as_deref();
        let confidence_scope = ConfidenceScope::new(
            args.confidence.as_deref(),
            args.exclude_external.unwrap_or(false),
            args.external_only.unwrap_or(false),
        );
        let related: Vec<GraphSymbol> = related
            .into_iter()
            .filter(|sym| language_matches(graph, sym, language_filter))
            .filter(|sym| confidence_scope.keeps(sym.confidence))
            .collect();

        // Page after filtering: skip `offset`, keep `max_results`.
        let offset = args.offset.unwrap_or(0);
        let truncated = related.len().saturating_sub(offset) > max_results;
        let selected: Vec<&GraphSymbol> = related.iter().skip(offset).take(max_results).collect();

        let packets: Vec<Value> = selected
            .iter()
            .map(|sym| symbol_packet(graph, sym, "inheritance_hierarchy", None))
            .collect();

        let direction = if member_mode {
            "member_implementations"
        } else if subtypes {
            if transitive_subtypes {
                "subtypes_transitive"
            } else {
                "subtypes"
            }
        } else {
            "supertypes"
        };
        let confidence_distribution = ToolCostHint::confidence_distribution_from_packets(&packets);
        let mut payload = graph_payload("inheritance_hierarchy", manager, refresh);
        payload.insert("root".to_string(), symbol_json(graph, &root_sym));
        payload.insert("direction".to_string(), json!(direction));
        payload.insert("offset".to_string(), json!(offset));
        payload.insert(
            "symbols".to_string(),
            json!(
                selected
                    .iter()
                    .map(|s| symbol_json(graph, s))
                    .collect::<Vec<_>>()
            ),
        );
        payload.insert("packets".to_string(), json!(packets));
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

    fn execute_impact_blocking(
        &self,
        call: &ToolCall,
        args: ImpactArgs,
        manager: &GraphManager,
        refresh: &squeezy_graph::RefreshReport,
    ) -> ToolResult {
        let graph = manager.graph();
        let max_results = graph_limit(args.max_results);

        // Collect the set of changed file IDs from the arguments.
        let mut changed: HashSet<FileId> = HashSet::new();

        if let Some(id) = args.symbol_id.as_deref()
            && let Some(sym) = graph.symbols.get(&SymbolId::new(id))
        {
            changed.insert(sym.file_id.clone());
        } else if let Some(q) = args.query.as_deref()
            && let Some(sym) = graph_symbol_search(graph, Some(q), None, None, None, None, None)
                .into_iter()
                .next()
        {
            changed.insert(sym.file_id.clone());
        }
        // Resolve a path filter into changed file IDs. An empty/whitespace
        // filter would otherwise match everything via the suffix/fuzzy fallback,
        // so skip it. Try an exact (case-insensitive) lookup before scanning so
        // a precise path resolves in O(1) without the boundary-aware fallback.
        let mut resolve_path = |filter: &str| {
            if filter.trim().is_empty() {
                return;
            }
            if let Some(file) = graph.find_file_case_insensitive(filter) {
                changed.insert(file.id.clone());
                return;
            }
            for file in graph.files.values() {
                if path_matches_filter(&file.relative_path, filter) {
                    changed.insert(file.id.clone());
                }
            }
        };
        if let Some(path) = args.path.as_deref() {
            resolve_path(path);
        }
        for extra in &args.extra_paths {
            resolve_path(extra.as_str());
        }

        if changed.is_empty() {
            let mut payload = graph_payload("impact", manager, refresh);
            payload.insert(
                "error".to_string(),
                json!("no symbol or file resolved; provide symbol_id, query, or path"),
            );
            payload.insert("packets".to_string(), json!([]));
            return make_result(
                call,
                ToolStatus::Success,
                Value::Object(payload),
                ToolCostHint::default(),
                None,
            );
        }

        // direct_only: return just the first-hop importer files of every
        // changed file, with no transitive fan-out and no
        // affected_symbols/affected_tests computation. Deterministic by sorting
        // on the relative path; each importer is emitted once across all roots.
        if args.direct_only.unwrap_or(false) {
            let mut importers: HashSet<FileId> = HashSet::new();
            for file_id in &changed {
                for importer in graph.direct_importers(file_id) {
                    // Don't list a changed file as an importer of itself.
                    if !changed.contains(&importer) {
                        importers.insert(importer);
                    }
                }
            }
            let mut importer_files: Vec<&squeezy_workspace::FileRecord> = importers
                .iter()
                .filter_map(|fid| graph.files.get(fid))
                .collect();
            importer_files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
            let total = importer_files.len();
            // Page after sorting: skip `offset`, keep `max_results`.
            let offset = args.offset.unwrap_or(0);
            let truncated = total.saturating_sub(offset) > max_results;
            let packets: Vec<Value> = importer_files
                .iter()
                .skip(offset)
                .take(max_results)
                .map(|file| {
                    json!({
                        "importer": {
                            "file_id": file.id.0,
                            "path": file.relative_path,
                        }
                    })
                })
                .collect();
            let returned = packets.len();
            let mut payload = graph_payload("impact", manager, refresh);
            payload.insert(
                "changed_files".to_string(),
                json!(
                    changed
                        .iter()
                        .filter_map(|fid| graph.files.get(fid))
                        .map(|f| f.relative_path.clone())
                        .collect::<Vec<_>>()
                ),
            );
            payload.insert("direct_only".to_string(), json!(true));
            payload.insert("offset".to_string(), json!(offset));
            payload.insert("packets".to_string(), json!(packets));
            payload.insert("importer_count".to_string(), json!(total));
            payload.insert("truncated".to_string(), json!(truncated));
            return make_result(
                call,
                ToolStatus::Success,
                Value::Object(payload),
                ToolCostHint {
                    matches_returned: returned as u64,
                    truncated,
                    ..ToolCostHint::default()
                },
                None,
            );
        }

        // All changed files are treated as potentially propagating for
        // worst-case impact; callers needing finer control can supply dirty
        // annotations through `annotate_dirty_ranges` first.
        let propagating = changed.clone();
        let removed: HashSet<FileId> = HashSet::new();
        let impact = graph.compute_impact(&changed, &propagating, &removed);

        // Scope the affected set by test-ness and language when requested.
        let exclude_tests = args.exclude_tests.unwrap_or(false);
        let tests_only = args.tests_only.unwrap_or(false);
        let language_filter = args.language.as_deref();
        let confidence_scope = ConfidenceScope::new(
            args.confidence.as_deref(),
            args.exclude_external.unwrap_or(false),
            args.external_only.unwrap_or(false),
        );
        let mut affected_symbols: Vec<&squeezy_graph::GraphSymbol> = impact
            .affected_symbols
            .iter()
            .filter(|sym| passes_test_scope(symbol_is_test(sym), exclude_tests, tests_only))
            .filter(|sym| language_matches(graph, sym, language_filter))
            .filter(|sym| confidence_scope.keeps(sym.confidence))
            .collect();
        // `compute_impact` collects from a HashMap, so iteration order is not
        // stable. Sort deterministically (path, span, id) before paging so the
        // `offset` window is reproducible across calls.
        affected_symbols.sort_by(|a, b| {
            a.file_id
                .0
                .cmp(&b.file_id.0)
                .then(a.span.start.line.cmp(&b.span.start.line))
                .then(a.span.start.column.cmp(&b.span.start.column))
                .then(a.id.0.cmp(&b.id.0))
        });
        // Page after sorting: skip `offset`, keep `max_results`.
        let offset = args.offset.unwrap_or(0);
        let truncated = affected_symbols.len().saturating_sub(offset) > max_results;
        let selected_symbols: Vec<&squeezy_graph::GraphSymbol> = affected_symbols
            .into_iter()
            .skip(offset)
            .take(max_results)
            .collect();

        let packets: Vec<Value> = selected_symbols
            .iter()
            .map(|sym| symbol_packet(graph, sym, "impact", None))
            .collect();

        let confidence_distribution = ToolCostHint::confidence_distribution_from_packets(&packets);

        let affected_files_json: Vec<Value> = impact
            .affected_files
            .iter()
            .filter_map(|fid| graph.files.get(fid))
            .map(|f| json!({"file_id": f.id.0, "path": f.relative_path}))
            .collect();

        let test_symbols_truncated = impact.affected_tests.len() > max_results;
        let test_symbols_json: Vec<Value> = impact
            .affected_tests
            .iter()
            .take(max_results)
            .map(|sym| symbol_json(graph, sym))
            .collect();

        let mut payload = graph_payload("impact", manager, refresh);
        payload.insert(
            "changed_files".to_string(),
            json!(
                changed
                    .iter()
                    .filter_map(|fid| graph.files.get(fid))
                    .map(|f| f.relative_path.clone())
                    .collect::<Vec<_>>()
            ),
        );
        payload.insert("affected_files".to_string(), json!(affected_files_json));
        payload.insert(
            "affected_file_count".to_string(),
            json!(impact.affected_files.len()),
        );
        payload.insert("offset".to_string(), json!(offset));
        payload.insert("packets".to_string(), json!(packets));
        payload.insert("test_symbols".to_string(), json!(test_symbols_json));
        payload.insert("truncated".to_string(), json!(truncated));
        if test_symbols_truncated {
            payload.insert("test_symbols_truncated".to_string(), json!(true));
        }
        make_result(
            call,
            ToolStatus::Success,
            Value::Object(payload),
            ToolCostHint {
                matches_returned: selected_symbols.len() as u64,
                truncated,
                confidence_distribution,
                ..ToolCostHint::default()
            },
            None,
        )
    }

    /// Resolve a source position (byte offset or 1-based line) inside a file to
    /// the smallest enclosing symbol. `byte` wins over `line` when both are
    /// present. On a hit, returns the standard symbol packet so the caller can
    /// chain straight into `symbol_context`/`read_slice`; on a miss, returns a
    /// `symbol: null` body with a `read_slice` next_action so the model can still
    /// read the requested slice.
    fn execute_symbol_at_blocking(
        &self,
        call: &ToolCall,
        args: SymbolAtArgs,
        manager: &GraphManager,
        refresh: &squeezy_graph::RefreshReport,
    ) -> ToolResult {
        let graph = manager.graph();
        // Resolve the path to an indexed file (case-insensitive exact first,
        // then the directory-aware fallback) and derive its FileId.
        let file = graph.find_file_case_insensitive(&args.path).or_else(|| {
            graph
                .files
                .values()
                .find(|file| path_matches_filter(&file.relative_path, &args.path))
        });
        let Some(file) = file else {
            let mut payload = graph_payload("symbol_at", manager, refresh);
            payload.insert(
                "fallback".to_string(),
                graph_zero_hit_fallback(graph, None, Some(&args.path), None, 0),
            );
            payload.insert("symbol".to_string(), Value::Null);
            payload.insert("packets".to_string(), json!([]));
            return make_result(
                call,
                ToolStatus::Success,
                Value::Object(payload),
                ToolCostHint::default(),
                None,
            );
        };
        let file_id = file.id.clone();
        let rel_path = file.relative_path.clone();

        let symbol = if let Some(byte) = args.byte {
            graph.symbol_at_byte(&file_id, byte)
        } else {
            // Default the line to 1 when neither byte nor line was supplied so a
            // bare `{path}` resolves the file's first enclosing symbol instead
            // of erroring.
            graph.symbol_at_line(&file_id, args.line.unwrap_or(1))
        };

        let mut payload = graph_payload("symbol_at", manager, refresh);
        payload.insert("path".to_string(), json!(rel_path));
        if let Some(byte) = args.byte {
            payload.insert("byte".to_string(), json!(byte));
        } else {
            payload.insert("line".to_string(), json!(args.line.unwrap_or(1)));
        }
        match symbol {
            Some(symbol) => {
                let packet = symbol_packet(graph, &symbol, "symbol_at", None);
                let confidence_distribution =
                    ToolCostHint::confidence_distribution_from(std::iter::once(symbol.confidence));
                payload.insert("symbol".to_string(), symbol_json(graph, &symbol));
                payload.insert("packets".to_string(), json!([packet]));
                make_result(
                    call,
                    ToolStatus::Success,
                    Value::Object(payload),
                    ToolCostHint {
                        matches_returned: 1,
                        confidence_distribution,
                        ..ToolCostHint::default()
                    },
                    None,
                )
            }
            None => {
                payload.insert("symbol".to_string(), Value::Null);
                payload.insert("packets".to_string(), json!([]));
                payload.insert("reason".to_string(), json!("no_enclosing_symbol"));
                // Steer the model to read the requested slice anyway. Anchor on
                // the line when one is known so `read_slice` lands at the cursor.
                let start_line = args.line.unwrap_or(1);
                payload.insert(
                    "next_action".to_string(),
                    json!({
                        "tool": "read_slice",
                        "arguments": {
                            "path": rel_path,
                            "start_line": start_line,
                        },
                    }),
                );
                make_result(
                    call,
                    ToolStatus::Success,
                    Value::Object(payload),
                    ToolCostHint::default(),
                    None,
                )
            }
        }
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
            // Mirror slice mode's policy-exclusion check so diff payloads carry
            // the same `ignored`/`ignored_reason` metadata. The file may not
            // exist on disk (deleted in the diff), in which case `read_prefix`
            // / `policy_exclusion_for_file` simply yield no reason.
            let ignored_reason = self
                .policy_exclusion_for_file(
                    &path,
                    &rel,
                    read_prefix(&path, POLICY_PREFIX_BYTES).ok().as_deref(),
                )
                .map(ExclusionReason::as_str);
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
                ignored_reason,
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
        let raw_content = String::from_utf8_lossy(&bytes).to_string();
        let content_sha256 = match sha256_file(&path) {
            Ok(hash) => hash,
            Err(err) => return tool_error(call, err),
        };
        // Resident-read dedup: if the model already read a byte window that
        // ENCLOSES this one from an unchanged file, it still has these exact
        // bytes in context — re-serializing them re-bills the whole transcript.
        // Recall-safe by the same contract grep/read_file dedup ship: suppress
        // only on an exact SHA match (file unchanged) AND a prior window that
        // fully contains the requested one. The stub names `same_as_call_id`
        // so the model can locate the resident bytes.
        let projected_end = offset.saturating_add(limit).min(total_bytes as usize);
        if let Some(store) = self.state_store.as_deref()
            && let Ok(snaps) = store.read_snapshots_for_path(rel_str.as_str())
        {
            let prior = snaps
                .iter()
                .filter(|snap| matches!(snap.tool_name.as_str(), "read_file" | "read_slice"))
                .filter(|snap| snap.content_sha256.as_deref() == Some(content_sha256.as_str()))
                .filter(|snap| {
                    snap.start_byte <= offset as u64 && snap.end_byte >= projected_end as u64
                })
                .max_by_key(|snap| snap.created_unix_millis);
            if let Some(snap) = prior {
                return make_result(
                    call,
                    ToolStatus::Success,
                    json!({
                        "tool": "read_slice",
                        "path": &rel_str,
                        "offset": offset,
                        "bytes_returned": 0,
                        "total_bytes": total_bytes,
                        "sha256": &content_sha256,
                        "unchanged": true,
                        "receipt_stub": true,
                        "dedup": true,
                        "resident_read": true,
                        "same_as_call_id": snap.call_id,
                        "same_as_tool_name": snap.tool_name,
                        "original_output_sha256": snap.stable_output_sha256,
                        "original_content_sha256": snap.content_sha256,
                        "original_model_output_bytes": snap.model_output_bytes,
                        "truncated": false,
                    }),
                    ToolCostHint::default(),
                    Some(content_sha256.clone()),
                );
            }
        }
        let start_line_1based: u32 = if let Some(span) = resolved_span {
            span.start.line.saturating_add(1)
        } else if offset == 0 {
            1
        } else {
            window_line_offset(&path, offset)
                .unwrap_or(0)
                .saturating_add(1)
        };
        let content = prefix_lines_with_numbers(&raw_content, start_line_1based);
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
        // Slice-mode wire payload now prefixes each line in `content` with
        // its 1-based absolute line number (cat -n format) so the model can
        // report line numbers without counting newlines. `start_line` is
        // included explicitly so the model can build line-relative offsets
        // without parsing the prefix. `bytes_returned` is always emitted —
        // when content is line-numbered, `content.len()` no longer matches
        // raw bytes read, so the compaction snapshot needs the explicit
        // byte count to size the window correctly.
        let mut payload = serde_json::Map::new();
        payload.insert("tool".to_string(), json!("read_slice"));
        payload.insert("path".to_string(), json!(&rel_str));
        payload.insert("offset".to_string(), json!(offset));
        payload.insert("start_line".to_string(), json!(start_line_1based));
        payload.insert("bytes_returned".to_string(), json!(bytes.len()));
        if truncated {
            payload.insert("truncated".to_string(), json!(true));
            payload.insert("total_bytes".to_string(), json!(total_bytes));
        }
        if let Some(reason) = ignored_reason {
            payload.insert("ignored".to_string(), json!(true));
            payload.insert("ignored_reason".to_string(), json!(reason));
        }
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
                if let Some(reason) = ctx.ignored_reason {
                    payload.insert("ignored".to_string(), json!(true));
                    payload.insert("ignored_reason".to_string(), json!(reason));
                }
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
                // A single hunk can yield many changed ranges, so the per-file
                // cap must be measured against the *range* count rather than the
                // hunk count. Capture the total before `take(max_ranges)` so the
                // `truncated` flag below reflects dropped ranges even when they
                // all live in one hunk.
                let total_changed_ranges = changed_ranges.len();
                for range in changed_ranges.into_iter().take(max_ranges) {
                    let bytes = text.as_bytes();
                    let capped_end = range
                        .end
                        .min(range.start.saturating_add(MAX_READ_LIMIT))
                        .min(bytes.len());
                    // Clamp the start too: a malformed range whose start lands
                    // past `capped_end` would otherwise panic the slice.
                    let content =
                        String::from_utf8_lossy(&bytes[range.start.min(capped_end)..capped_end])
                            .to_string();
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
                truncated |= file.patch_truncated || total_changed_ranges > max_ranges;
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
        // Mirror slice mode's ignored-file metadata so a policy-excluded file
        // read in diff mode is distinguishable from a clean one.
        if let Some(reason) = ctx.ignored_reason {
            payload.insert("ignored".to_string(), json!(true));
            payload.insert("ignored_reason".to_string(), json!(reason));
        }
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
            ..
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
        // `snapshot.content` is the model-facing, line-numbered render
        // (`"{line_no}\t{source}"` from `prefix_lines_with_numbers`). Diffing it
        // directly against the raw file bytes treats every line-number gutter as
        // a change, producing spurious ranges for an otherwise-unchanged file.
        // Strip the gutter back to source bytes before diffing.
        let baseline_content = strip_line_number_prefixes(&snapshot.content);
        let local_ranges = byte_diff_ranges(baseline_content.as_bytes(), current.as_bytes());
        let mut cost = ToolCostHint::default();
        let mut ranges = Vec::new();
        let mut packets = Vec::new();
        for range in local_ranges
            .into_iter()
            .take(args.max_ranges.unwrap_or(20).clamp(1, 100))
        {
            let start = offset.saturating_add(range.start);
            let end_bytes = offset.saturating_add(range.end);
            let capped_end = range
                .end
                .min(range.start.saturating_add(MAX_READ_LIMIT))
                .min(current.len());
            // Clamp the start too so a malformed range can't panic the slice.
            let content = String::from_utf8_lossy(
                &current.as_bytes()[range.start.min(capped_end)..capped_end],
            )
            .to_string();
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
        let graph_ready = self.wait_for_graph_ready(graph_ready_wait());
        // Recover a poisoned guard instead of permanently failing impact lookups.
        let mut graph = self.graph.lock().unwrap_or_else(|e| e.into_inner());
        let Some(manager) = graph.as_mut() else {
            return if !graph_ready {
                json!({
                    "available": false,
                    "status": "graph_indexing",
                    "retryable": true,
                    "reason": "semantic graph is still being indexed",
                })
            } else {
                json!({
                    "available": false,
                    "status": "graph_unavailable",
                    "retryable": false,
                    "reason": "semantic graph is unavailable for this workspace",
                })
            };
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
        // Trim policy: `refresh` and `coverage` are dropped from the wire
        // payload for the same reason as `graph_payload` — the model never
        // branched on either. The refresh side-effect still runs and still
        // emits its typed graph event.
        let _ = refresh;
        let mut payload = serde_json::Map::new();
        payload.insert("available".to_string(), json!(true));
        payload.insert("files".to_string(), json!(files));
        Value::Object(payload)
    }
}

#[cfg(test)]
#[path = "graph_tools_tests.rs"]
mod tests;
