use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use ignore::WalkBuilder;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use squeezy_core::PermissionCapability;
use squeezy_vcs::{DiffMode, DiffOptions};
use squeezy_workspace::ExclusionReason;
use tokio_util::sync::CancellationToken;

use crate::{
    DEFAULT_MAX_BYTES_PER_FILE, DEFAULT_MAX_FILES, DEFAULT_READ_LIMIT, MAX_READ_LIMIT,
    POLICY_PREFIX_BYTES, ToolCall, ToolCostHint, ToolOutputReplayKey, ToolOutputReplayServed,
    ToolOutputReplaySource, ToolRegistry, ToolResult, ToolStatus, build_include_set,
    build_required_glob, diff_path_set, file_len, is_secret_path, make_result, read_prefix,
    read_range, sha256_file, tool_arg_error, tool_error, truncate_text, workspace_path,
};

pub(crate) const DEFAULT_MAX_MATCHES: usize = 250;
pub(crate) const DEFAULT_OUTPUT_BYTE_CAP: usize = 48_000;
pub(crate) const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Detect the canonical image MIME type from a byte prefix using magic
/// numbers. Supports PNG, JPEG, GIF (87a/89a), and WEBP (RIFF / WEBP
/// container) — the set of formats the upstream vision providers
/// (Anthropic, OpenAI, Google, Bedrock) all accept. Returns `None` when
/// the prefix does not match a known image format so the caller can
/// fall back to the default text path.
pub(crate) fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GlobArgs {
    pub(crate) pattern: String,
    pub(crate) path: Option<String>,
    include_ignored: Option<bool>,
    diff_only: Option<bool>,
    max_paths: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrepArgs {
    pub(crate) pattern: String,
    pub(crate) path: Option<String>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    include_ignored: Option<bool>,
    diff_only: Option<bool>,
    output_mode: Option<GrepOutputMode>,
    max_files: Option<usize>,
    max_bytes_per_file: Option<usize>,
    max_matches: Option<usize>,
    output_byte_cap: Option<usize>,
    offset: Option<usize>,
    /// F13: optional number of leading + trailing context lines to emit
    /// around each match (like `rg -C N`). 0 (default) preserves the
    /// pre-F13 behavior of returning only the matching line. Clamped to
    /// `MAX_GREP_CONTEXT` defensively in case a non-spec caller sends a
    /// larger value.
    context: Option<u32>,
}

/// Hard cap on grep `context` to keep per-match windows bounded even if
/// a caller bypasses the JSON-schema `maximum` (e.g. an external client
/// that does not re-validate). Mirrors the schema's `"maximum": 50`.
pub(crate) const MAX_GREP_CONTEXT: u32 = 50;

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GrepOutputMode {
    #[default]
    Content,
    FilesWithMatches,
    Count,
}

impl GrepOutputMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::FilesWithMatches => "files_with_matches",
            Self::Count => "count",
        }
    }

    const fn is_limited(self, matches: usize, paths: usize, limit: usize) -> bool {
        match self {
            Self::Content => matches >= limit,
            Self::FilesWithMatches => paths >= limit,
            Self::Count => false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadFileArgs {
    pub(crate) path: String,
    offset: Option<usize>,
    limit: Option<usize>,
    diff_only: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadToolOutputArgs {
    /// sha256-keyed handle minted by [`ToolOutputStore::maybe_spill`] for
    /// any oversized tool result. Mutually exclusive with `path`.
    pub(crate) handle: Option<String>,
    /// Spillover tempfile path minted by the shell tool when its
    /// in-memory output overflows the truncation budget. Must point
    /// inside the per-session spillover directory; arbitrary
    /// filesystem locations are rejected. Mutually exclusive with
    /// `handle`.
    pub(crate) path: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
}

pub(crate) fn contains_skipped_dir(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|part| matches!(part, ".git" | ".hg" | ".svn" | ".squeezy"))
    })
}

impl ToolRegistry {
    pub(crate) async fn execute_glob(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
    ) -> ToolResult {
        let args = match serde_json::from_value::<GlobArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let start = match self.resolve_existing(args.path.as_deref().unwrap_or(".")) {
            Ok(path) => path,
            Err(err) => return tool_error(call, err),
        };
        let pattern = match build_required_glob(&args.pattern) {
            Ok(pattern) => pattern,
            Err(err) => return tool_error(call, err),
        };
        let include_ignored = args.include_ignored.unwrap_or(false);
        let diff_only = args.diff_only.unwrap_or(false);
        let diff_paths = if diff_only {
            diff_path_set(&self.diff_snapshot(DiffMode::Worktree, DiffOptions::default()))
        } else {
            BTreeSet::new()
        };
        let max_paths = args.max_paths.unwrap_or(DEFAULT_MAX_MATCHES).min(1_000);
        let offset = args.offset.unwrap_or(0);

        let mut builder = WalkBuilder::new(&start);
        builder
            .follow_links(false)
            .hidden(false)
            .ignore(!include_ignored)
            .git_ignore(!include_ignored)
            .git_exclude(!include_ignored)
            .require_git(false)
            .parents(true)
            .sort_by_file_path(|left, right| left.cmp(right));

        let mut paths = Vec::new();
        let mut skipped_paths = 0usize;
        let mut skipped_secret_files = 0u64;
        let mut cost = ToolCostHint::default();

        for entry in builder.build() {
            if cancel.is_cancelled() {
                return ToolResult::cancelled(call);
            }
            if paths.len() >= max_paths {
                cost.truncated = true;
                break;
            }

            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let path = entry.path();
            if !path.is_file() || contains_skipped_dir(path) {
                continue;
            }
            let rel = self.relative(path);
            if !include_ignored && self.policy_exclusion_for_file(path, &rel, None).is_some() {
                continue;
            }
            let rel_str = workspace_path(&rel);
            if diff_only && !diff_paths.contains(rel_str.as_str()) {
                continue;
            }
            if is_secret_path(&rel) {
                skipped_secret_files += 1;
                continue;
            }
            cost.files_scanned += 1;
            if !pattern.is_match(rel.as_path()) {
                continue;
            }
            if skipped_paths < offset {
                skipped_paths += 1;
                continue;
            }
            paths.push(json!(rel_str));
            cost.matches_returned += 1;
        }

        make_result(
            call,
            ToolStatus::Success,
            json!({
                "paths": paths,
                "metadata": {
                    "pattern": args.pattern,
                    "path": args.path.as_deref().unwrap_or("."),
                    "include_ignored": include_ignored,
                    "diff_only": diff_only,
                    "offset": offset,
                    "skipped_secret_files": skipped_secret_files,
                },
            }),
            cost,
            None,
        )
    }

    pub(crate) async fn execute_grep(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
    ) -> ToolResult {
        let args = match serde_json::from_value::<GrepArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };

        let regex = match Regex::new(&args.pattern) {
            Ok(regex) => regex,
            Err(err) => {
                return make_result(
                    call,
                    ToolStatus::Error,
                    json!({ "error": format!("invalid regex: {err}") }),
                    ToolCostHint::default(),
                    None,
                );
            }
        };

        let start = match self.resolve_existing(args.path.as_deref().unwrap_or(".")) {
            Ok(path) => path,
            Err(err) => return tool_error(call, err),
        };

        let include = match build_include_set(args.include.as_deref()) {
            Ok(include) => include,
            Err(err) => return tool_error(call, err),
        };

        let exclude = match build_include_set(args.exclude.as_deref()) {
            Ok(exclude) => exclude,
            Err(err) => return tool_error(call, err),
        };

        let include_ignored = args.include_ignored.unwrap_or(false);
        let diff_only = args.diff_only.unwrap_or(false);
        let diff_paths = if diff_only {
            diff_path_set(&self.diff_snapshot(DiffMode::Worktree, DiffOptions::default()))
        } else {
            BTreeSet::new()
        };
        let output_mode = args.output_mode.unwrap_or_default();
        let max_files = args
            .max_files
            .unwrap_or(DEFAULT_MAX_FILES)
            .min(DEFAULT_MAX_FILES);
        let max_bytes_per_file = args
            .max_bytes_per_file
            .unwrap_or(DEFAULT_MAX_BYTES_PER_FILE)
            .min(DEFAULT_MAX_BYTES_PER_FILE);
        let max_matches = args.max_matches.unwrap_or(DEFAULT_MAX_MATCHES).min(1_000);
        let offset = args.offset.unwrap_or(0);
        let output_byte_cap = args
            .output_byte_cap
            .unwrap_or(DEFAULT_OUTPUT_BYTE_CAP)
            .min(128_000);
        let context = args.context.unwrap_or(0).min(MAX_GREP_CONTEXT) as usize;

        // Cross-tool "already-resident" dedup: when the grep target is a
        // single file the model already read in full this session (a
        // `read_file`/`read_slice` snapshot whose `content_sha256` still
        // matches the file on disk and whose byte window fully covers the
        // file), run the regex IN-MEMORY against the stored source and
        // return a receipt carrying only the matched line numbers + text.
        // This avoids re-walking disk and, more importantly, avoids
        // re-emitting bytes that are already resident in the model's
        // context (which would otherwise be re-billed as cache_write).
        // Provably equivalent: the regex runs over identical content, so
        // the matches equal a disk grep's. Gated to the plain content
        // path (no include/exclude/diff_only/context filters) so the
        // short-circuit can never change the result set; anything outside
        // that gate falls through to the normal disk grep below.
        if matches!(output_mode, GrepOutputMode::Content)
            && context == 0
            && !diff_only
            && include.is_none()
            && exclude.is_none()
            && start.is_file()
            && let Some(result) = self.grep_resident_snapshot_receipt(
                call,
                &start,
                &regex,
                offset,
                max_matches,
                output_byte_cap,
            )
        {
            return result;
        }

        let mut builder = WalkBuilder::new(&start);
        builder
            .follow_links(false)
            .hidden(false)
            .ignore(!include_ignored)
            .git_ignore(!include_ignored)
            .git_exclude(!include_ignored)
            .require_git(false)
            .parents(true)
            .sort_by_file_path(|left, right| left.cmp(right));

        let mut matches = Vec::new();
        let mut paths = BTreeSet::new();
        let mut count = 0u64;
        let mut skipped_matches = 0usize;
        let mut cost = ToolCostHint::default();
        let mut skipped_secret_files = 0u64;
        let mut scanned_files = 0usize;
        let mut stop_search = false;

        for entry in builder.build() {
            if cancel.is_cancelled() {
                return ToolResult::cancelled(call);
            }
            if scanned_files >= max_files
                || output_mode.is_limited(matches.len(), paths.len(), max_matches)
                || stop_search
            {
                cost.truncated = true;
                break;
            }

            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let path = entry.path();
            if !path.is_file() || contains_skipped_dir(path) {
                continue;
            }
            let rel = self.relative(path);
            if !include_ignored && self.policy_exclusion_for_file(path, &rel, None).is_some() {
                continue;
            }
            let rel_str = workspace_path(&rel);
            if diff_only && !diff_paths.contains(rel_str.as_str()) {
                continue;
            }
            if include
                .as_ref()
                .is_some_and(|include| !include.is_match(rel.as_path()))
            {
                continue;
            }
            if exclude
                .as_ref()
                .is_some_and(|exclude| exclude.is_match(rel.as_path()))
            {
                continue;
            }
            if is_secret_path(&rel) {
                skipped_secret_files += 1;
                continue;
            }

            scanned_files += 1;
            cost.files_scanned += 1;
            let bytes = match read_prefix(path, max_bytes_per_file) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            if !include_ignored {
                let head_len = bytes.len().min(POLICY_PREFIX_BYTES);
                if self
                    .policy_exclusion_for_file(path, &rel, Some(&bytes[..head_len]))
                    .is_some()
                {
                    continue;
                }
            }
            cost.bytes_read += bytes.len() as u64;
            let file_truncated = file_len(path)
                .map(|len| len > bytes.len() as u64)
                .unwrap_or(false);
            if file_truncated {
                cost.truncated = true;
            }

            let text = String::from_utf8_lossy(&bytes);
            let needs_context = context > 0 && matches!(output_mode, GrepOutputMode::Content);
            if !needs_context {
                for (line_index, line) in text.lines().enumerate() {
                    if !regex.is_match(line) {
                        continue;
                    }
                    if skipped_matches < offset {
                        skipped_matches += 1;
                        continue;
                    }
                    count += 1;
                    match output_mode {
                        GrepOutputMode::Content => {
                            let next = json!({
                                "path": &rel_str,
                                "line": line_index + 1,
                                "text": truncate_text(line, 2000),
                            });
                            let next_len =
                                serde_json::to_string(&next).map_or(0, |text| text.len());
                            if cost.output_bytes + next_len as u64 > output_byte_cap as u64 {
                                cost.truncated = true;
                                stop_search = true;
                                break;
                            }
                            cost.output_bytes += next_len as u64;
                            cost.matches_returned += 1;
                            matches.push(next);
                        }
                        GrepOutputMode::FilesWithMatches => {
                            if paths.insert(rel_str.clone()) {
                                cost.matches_returned += 1;
                            }
                        }
                        GrepOutputMode::Count => {
                            cost.matches_returned = count;
                        }
                    }
                    if output_mode.is_limited(matches.len(), paths.len(), max_matches) {
                        cost.truncated = true;
                        stop_search = true;
                        break;
                    }
                    if matches!(output_mode, GrepOutputMode::FilesWithMatches) {
                        // The file is already recorded; files-with-matches only
                        // reports each path once, so skip its remaining lines
                        // instead of re-running the regex and re-cloning the path.
                        break;
                    }
                }
            } else {
                let lines: Vec<&str> = text.lines().collect();
                for (line_index, line) in lines.iter().enumerate() {
                    if !regex.is_match(line) {
                        continue;
                    }
                    if skipped_matches < offset {
                        skipped_matches += 1;
                        continue;
                    }
                    count += 1;
                    match output_mode {
                        GrepOutputMode::Content => {
                            let line_text = truncate_text(line, 2000);
                            let mut next = serde_json::Map::new();
                            next.insert("path".to_string(), json!(&rel_str));
                            next.insert("line".to_string(), json!(line_index + 1));
                            next.insert("text".to_string(), json!(line_text));
                            let before_start = line_index.saturating_sub(context);
                            let before_lines: Vec<Value> = lines[before_start..line_index]
                                .iter()
                                .enumerate()
                                .map(|(offset_idx, ctx_line)| {
                                    json!({
                                        "line": before_start + offset_idx + 1,
                                        "text": truncate_text(ctx_line, 2000),
                                    })
                                })
                                .collect();
                            let after_end = line_index
                                .saturating_add(1)
                                .saturating_add(context)
                                .min(lines.len());
                            let after_lines: Vec<Value> = lines[line_index + 1..after_end]
                                .iter()
                                .enumerate()
                                .map(|(offset_idx, ctx_line)| {
                                    json!({
                                        "line": line_index + 2 + offset_idx,
                                        "text": truncate_text(ctx_line, 2000),
                                    })
                                })
                                .collect();
                            next.insert("context_before".to_string(), Value::Array(before_lines));
                            next.insert("context_after".to_string(), Value::Array(after_lines));
                            let next = Value::Object(next);
                            let next_len =
                                serde_json::to_string(&next).map_or(0, |text| text.len());
                            if cost.output_bytes + next_len as u64 > output_byte_cap as u64 {
                                cost.truncated = true;
                                stop_search = true;
                                break;
                            }
                            cost.output_bytes += next_len as u64;
                            cost.matches_returned += 1;
                            matches.push(next);
                        }
                        GrepOutputMode::FilesWithMatches => {
                            if paths.insert(rel_str.clone()) {
                                cost.matches_returned += 1;
                            }
                        }
                        GrepOutputMode::Count => {
                            cost.matches_returned = count;
                        }
                    }
                    if output_mode.is_limited(matches.len(), paths.len(), max_matches) {
                        cost.truncated = true;
                        stop_search = true;
                        break;
                    }
                    if matches!(output_mode, GrepOutputMode::FilesWithMatches) {
                        // The file is already recorded; files-with-matches only
                        // reports each path once, so skip its remaining lines
                        // instead of re-running the regex and re-cloning the path.
                        break;
                    }
                }
            }
        }

        let mut metadata = BTreeMap::new();
        metadata.insert("pattern".to_string(), json!(args.pattern));
        metadata.insert(
            "path".to_string(),
            json!(args.path.as_deref().unwrap_or(".")),
        );
        if let Some(include) = args.include.as_ref() {
            metadata.insert("include".to_string(), json!(include));
        }
        if let Some(exclude) = args.exclude.as_ref() {
            metadata.insert("exclude".to_string(), json!(exclude));
        }
        metadata.insert("include_ignored".to_string(), json!(include_ignored));
        metadata.insert("diff_only".to_string(), json!(diff_only));
        metadata.insert("output_mode".to_string(), json!(output_mode.as_str()));
        metadata.insert("offset".to_string(), json!(offset));
        metadata.insert("context".to_string(), json!(context));
        metadata.insert(
            "skipped_secret_files".to_string(),
            json!(skipped_secret_files),
        );
        if !include_ignored {
            metadata.insert(
                "hint".to_string(),
                json!(
                    "ignored paths were skipped; retry with include_ignored=true only when needed"
                ),
            );
        }

        let content = match output_mode {
            GrepOutputMode::Content => json!({
                "matches": matches,
                "metadata": metadata,
            }),
            GrepOutputMode::FilesWithMatches => json!({
                "paths": paths.into_iter().collect::<Vec<_>>(),
                "metadata": metadata,
            }),
            GrepOutputMode::Count => json!({
                "count": count,
                "metadata": metadata,
            }),
        };

        make_result(call, ToolStatus::Success, content, cost, None)
    }

    /// Cross-tool "already-resident" grep dedup for a single-file target.
    ///
    /// When `path` was already read in full this session and is unchanged
    /// on disk, run `regex` over the stored source in memory and return a
    /// content-mode receipt carrying only the matched line numbers + text.
    /// Returns `None` (so the caller falls through to the disk grep) when
    /// there is no state store, no covering snapshot, the file changed
    /// (SHA mismatch), or the snapshot only covers part of the file —
    /// correctness comes first, so any uncertainty defers to disk.
    fn grep_resident_snapshot_receipt(
        &self,
        call: &ToolCall,
        path: &Path,
        regex: &Regex,
        offset: usize,
        max_matches: usize,
        output_byte_cap: usize,
    ) -> Option<ToolResult> {
        let store = self.state_store.as_deref()?;
        let rel = self.relative(path);
        if is_secret_path(&rel) {
            return None;
        }
        let rel_str = workspace_path(&rel);

        let total_bytes = file_len(path).ok()?;
        let content_sha256 = sha256_file(path).ok()?;
        let snapshots = store.read_snapshots_for_path(rel_str.as_str()).ok()?;

        // Require an exact SHA match (file unchanged since the read) AND a
        // window that starts at byte 0 and reaches end-of-file, so the
        // stored content is the whole file the regex would scan on disk.
        // Anything narrower falls through.
        let snap = snapshots
            .iter()
            .filter(|snap| matches!(snap.tool_name.as_str(), "read_file" | "read_slice"))
            .filter(|snap| snap.content_sha256.as_deref() == Some(content_sha256.as_str()))
            .filter(|snap| snap.start_byte == 0 && snap.end_byte >= total_bytes)
            .max_by_key(|snap| snap.created_unix_millis)?;

        // The stored `content` is the model-facing, line-numbered render
        // (`"{line_no}\t{source}"` from `prefix_lines_with_numbers`). Strip
        // the `"{N}\t"` prefix so the regex matches the source — not the
        // line-number gutter — and reuse the embedded line number, which is
        // the authoritative 1-based number the model already saw.
        let mut matches = Vec::new();
        let mut count = 0u64;
        let mut skipped_matches = 0usize;
        let mut cost = ToolCostHint {
            files_scanned: 1,
            bytes_read: total_bytes,
            ..ToolCostHint::default()
        };
        let mut truncated = false;
        for raw_line in snap.content.lines() {
            let (line_no, source) = match raw_line.split_once('\t') {
                Some((number, rest)) if number.parse::<usize>().is_ok() => {
                    (number.parse::<usize>().unwrap_or(0), rest)
                }
                // A line without the expected `"{N}\t"` gutter means the
                // stored content is not the line-numbered render we rely on
                // here; bail out to the disk grep rather than guess at line
                // numbers.
                _ => return None,
            };
            if !regex.is_match(source) {
                continue;
            }
            if skipped_matches < offset {
                skipped_matches += 1;
                continue;
            }
            count += 1;
            let next = json!({
                "path": &rel_str,
                "line": line_no,
                "text": truncate_text(source, 2000),
            });
            let next_len = serde_json::to_string(&next).map_or(0, |text| text.len());
            if cost.output_bytes + next_len as u64 > output_byte_cap as u64 {
                truncated = true;
                break;
            }
            cost.output_bytes += next_len as u64;
            cost.matches_returned += 1;
            matches.push(next);
            if matches.len() >= max_matches {
                truncated = true;
                break;
            }
        }
        cost.truncated = truncated;

        let mut metadata = BTreeMap::new();
        metadata.insert("pattern".to_string(), json!(regex.as_str()));
        metadata.insert("path".to_string(), json!(&rel_str));
        metadata.insert("output_mode".to_string(), json!("content"));
        metadata.insert("offset".to_string(), json!(offset));
        metadata.insert("context".to_string(), json!(0));
        metadata.insert("count".to_string(), json!(count));
        // Reuse the existing receipt envelope so downstream dedup / packing
        // treats this like any other already-resident receipt.
        metadata.insert("receipt_stub".to_string(), json!(true));
        metadata.insert("dedup".to_string(), json!(true));
        metadata.insert("resident_read".to_string(), json!(true));
        metadata.insert("same_as_call_id".to_string(), json!(snap.call_id));
        metadata.insert("same_as_tool_name".to_string(), json!(snap.tool_name));

        let content = json!({
            "matches": matches,
            "metadata": metadata,
        });
        Some(make_result(call, ToolStatus::Success, content, cost, None))
    }

    pub(crate) async fn execute_read_file(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<ReadFileArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let path = match self.resolve_existing_for_call(
            &args.path,
            &call.call_id,
            PermissionCapability::Read,
        ) {
            Ok(path) => path,
            Err(err) => return tool_error(call, err),
        };
        let rel = self.relative(&path);
        let rel_str = workspace_path(&rel);
        if args.diff_only.unwrap_or(false) {
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

        let total_bytes = match file_len(&path) {
            Ok(len) => len,
            Err(err) => return tool_error(call, err),
        };
        let prefix_bytes = read_prefix(&path, POLICY_PREFIX_BYTES).ok();
        let ignored_reason = self
            .policy_exclusion_for_file(&path, &rel, prefix_bytes.as_deref())
            .map(ExclusionReason::as_str);

        // F18: detect image MIME via magic bytes on the prefix we already
        // read for policy checks. PNG, JPEG, GIF, and WEBP all surface in
        // the first 12 bytes, so the policy-prefix read covers detection
        // without an extra syscall. When the file is an image, return a
        // structured payload (path / mime / base64 data / sha256) so the
        // agent can wrap the bytes in `LlmInputItem::Image` instead of
        // re-serialising binary content as lossy UTF-8 text.
        if let Some(mime) = prefix_bytes.as_deref().and_then(detect_image_mime) {
            if total_bytes > MAX_IMAGE_BYTES {
                return make_result(
                    call,
                    ToolStatus::Error,
                    json!({
                        "error": format!(
                            "image too large to inline: {} is {} bytes, cap is {} bytes",
                            rel_str, total_bytes, MAX_IMAGE_BYTES
                        ),
                        "path": &rel_str,
                        "image": true,
                        "mime_type": mime,
                        "total_bytes": total_bytes,
                        "max_image_bytes": MAX_IMAGE_BYTES,
                    }),
                    ToolCostHint::default(),
                    None,
                );
            }
            let bytes = match read_range(&path, 0, total_bytes as usize) {
                Ok(bytes) => bytes,
                Err(err) => return tool_error(call, err),
            };
            let content_sha256 = match sha256_file(&path) {
                Ok(hash) => hash,
                Err(err) => return tool_error(call, err),
            };
            let data_base64 = BASE64_STANDARD.encode(&bytes);
            let encoded_len = data_base64.len() as u64;
            let mut payload = serde_json::Map::new();
            payload.insert("path".to_string(), json!(&rel_str));
            payload.insert("image".to_string(), json!(true));
            payload.insert("mime_type".to_string(), json!(mime));
            payload.insert("total_bytes".to_string(), json!(total_bytes));
            payload.insert("sha256".to_string(), json!(&content_sha256));
            payload.insert("data_base64".to_string(), Value::String(data_base64));
            if let Some(reason) = ignored_reason {
                payload.insert("ignored".to_string(), json!(true));
                payload.insert("ignored_reason".to_string(), json!(reason));
            }
            let cost = ToolCostHint {
                bytes_read: total_bytes,
                output_bytes: encoded_len,
                truncated: false,
                ..ToolCostHint::default()
            };
            return make_result(
                call,
                ToolStatus::Success,
                Value::Object(payload),
                cost,
                Some(content_sha256),
            );
        }

        let offset = args.offset.unwrap_or(0).min(total_bytes as usize);
        let limit = args.limit.unwrap_or(DEFAULT_READ_LIMIT).min(MAX_READ_LIMIT);

        // F03: dedup against the last receipt for this (path, offset, end)
        // window. Mirror the pattern used by `read_slice_last_receipt_diff`:
        // if the full-file hash matches what we already returned for the same
        // window in a prior call, emit a stub instead of re-serializing
        // identical bytes.
        let content_sha256 = match sha256_file(&path) {
            Ok(hash) => hash,
            Err(err) => return tool_error(call, err),
        };
        let projected_end = offset.saturating_add(limit).min(total_bytes as usize);
        if let Some(store) = self.state_store.as_deref()
            && let Ok(snapshots) = store.read_snapshots_for_path(rel_str.as_str())
        {
            let prior = snapshots
                .iter()
                .filter(|snap| {
                    snap.start_byte == offset as u64
                        && snap.end_byte == projected_end as u64
                        && snap.tool_name == "read_file"
                })
                .filter(|snap| snap.content_sha256.as_deref() == Some(content_sha256.as_str()))
                .max_by_key(|snap| snap.created_unix_millis);
            if let Some(snap) = prior {
                return make_result(
                    call,
                    ToolStatus::Success,
                    json!({
                        "tool": "read_file",
                        "path": &rel_str,
                        "offset": offset,
                        "bytes_returned": 0,
                        "total_bytes": total_bytes,
                        "sha256": &content_sha256,
                        "unchanged": true,
                        "receipt_stub": true,
                        "dedup": true,
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

        let bytes = match read_range(&path, offset as u64, limit) {
            Ok(bytes) => bytes,
            Err(err) => return tool_error(call, err),
        };
        let end = offset.saturating_add(bytes.len());
        let raw_content = String::from_utf8_lossy(&bytes).to_string();
        let start_line_1based: u32 = if offset == 0 {
            1
        } else {
            crate::graph_tools::window_line_offset(&path, offset)
                .unwrap_or(0)
                .saturating_add(1)
        };
        let content =
            crate::graph_tools::prefix_lines_with_numbers(&raw_content, start_line_1based);
        let cost = ToolCostHint {
            bytes_read: total_bytes,
            output_bytes: content.len() as u64,
            truncated: end < total_bytes as usize,
            ..ToolCostHint::default()
        };

        let mut payload = serde_json::Map::new();
        payload.insert("path".to_string(), json!(&rel_str));
        payload.insert("offset".to_string(), json!(offset));
        payload.insert("start_line".to_string(), json!(start_line_1based));
        payload.insert("bytes_returned".to_string(), json!(bytes.len()));
        payload.insert("total_bytes".to_string(), json!(total_bytes));
        payload.insert("sha256".to_string(), json!(content_sha256));
        payload.insert("truncated".to_string(), json!(end < total_bytes as usize));
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

    pub(crate) async fn execute_read_tool_output(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<ReadToolOutputArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let offset = args.offset.unwrap_or(0);
        let limit = args.limit.unwrap_or(DEFAULT_READ_LIMIT).min(MAX_READ_LIMIT);
        match (args.handle.as_deref(), args.path.as_deref()) {
            (Some(handle), None) => self.read_tool_output_by_handle(call, handle, offset, limit),
            (None, Some(path)) => self.read_tool_output_by_path(call, path, offset, limit),
            (Some(_), Some(_)) => tool_error(
                call,
                "invalid tool arguments: read_tool_output accepts exactly one of `handle` or `path`",
            ),
            (None, None) => tool_error(
                call,
                "invalid tool arguments: read_tool_output requires either `handle` or `path`",
            ),
        }
    }

    fn read_tool_output_by_handle(
        &self,
        call: &ToolCall,
        handle: &str,
        offset: usize,
        limit: usize,
    ) -> ToolResult {
        let key = ToolOutputReplayKey {
            source: ToolOutputReplaySource::Handle(handle.to_string()),
            offset,
            limit,
        };
        if let Some(stub) = self.read_tool_output_replay_stub(call, &key) {
            return stub;
        }
        let output = match self.output_store.read(handle, offset, limit) {
            Ok(output) => output,
            Err(err) => return tool_error(call, err),
        };
        let cost = ToolCostHint {
            bytes_read: output.bytes_returned as u64,
            output_bytes: output.content.len() as u64,
            truncated: output.truncated,
            ..ToolCostHint::default()
        };
        self.remember_read_tool_output(
            key,
            call.call_id.as_str(),
            output.bytes_returned,
            output.sha256.as_str(),
        );
        make_result(
            call,
            ToolStatus::Success,
            json!({
                "handle": handle,
                "offset": output.offset,
                "bytes_returned": output.bytes_returned,
                "total_bytes": output.total_bytes,
                "sha256": output.sha256,
                "truncated": output.truncated,
                "content": output.content,
            }),
            cost,
            None,
        )
    }

    fn read_tool_output_by_path(
        &self,
        call: &ToolCall,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> ToolResult {
        let key = ToolOutputReplayKey {
            source: ToolOutputReplaySource::Path(path.to_string()),
            offset,
            limit,
        };
        if let Some(stub) = self.read_tool_output_replay_stub(call, &key) {
            return stub;
        }
        let output = match self.shell_spillover.read_range(path, offset, limit) {
            Ok(output) => output,
            Err(err) => return tool_error(call, err),
        };
        let cost = ToolCostHint {
            bytes_read: output.bytes_returned as u64,
            output_bytes: output.content.len() as u64,
            truncated: output.truncated,
            ..ToolCostHint::default()
        };
        self.remember_read_tool_output(
            key,
            call.call_id.as_str(),
            output.bytes_returned,
            output.sha256.as_str(),
        );
        make_result(
            call,
            ToolStatus::Success,
            json!({
                "path": output.path.to_string_lossy(),
                "offset": output.offset,
                "bytes_returned": output.bytes_returned,
                "total_bytes": output.total_bytes,
                "sha256": output.sha256,
                "truncated": output.truncated,
                "content": output.content,
            }),
            cost,
            None,
        )
    }

    /// On a repeat fetch of the same `(handle_or_path, offset, limit)`,
    /// emit a brief receipt stub instead of re-serializing the bytes.
    fn read_tool_output_replay_stub(
        &self,
        call: &ToolCall,
        key: &ToolOutputReplayKey,
    ) -> Option<ToolResult> {
        let served = self
            .tool_output_replay_seen
            .lock()
            .ok()
            .and_then(|guard| guard.get(key).cloned())?;
        let mut content = serde_json::Map::new();
        content.insert("receipt_stub".to_string(), Value::Bool(true));
        content.insert(
            "same_as_call_id".to_string(),
            Value::String(served.call_id.clone()),
        );
        content.insert("unchanged".to_string(), Value::Bool(true));
        content.insert(
            "size_bytes".to_string(),
            Value::Number(serde_json::Number::from(served.size_bytes)),
        );
        content.insert(
            "sha256_short".to_string(),
            Value::String(served.sha256_short.clone()),
        );
        match &key.source {
            ToolOutputReplaySource::Handle(handle) => {
                content.insert("handle".to_string(), Value::String(handle.clone()));
            }
            ToolOutputReplaySource::Path(path) => {
                content.insert("path".to_string(), Value::String(path.clone()));
            }
        }
        content.insert(
            "offset".to_string(),
            Value::Number(serde_json::Number::from(key.offset)),
        );
        content.insert(
            "limit".to_string(),
            Value::Number(serde_json::Number::from(key.limit)),
        );
        Some(make_result(
            call,
            ToolStatus::Success,
            Value::Object(content),
            ToolCostHint::default(),
            None,
        ))
    }

    fn remember_read_tool_output(
        &self,
        key: ToolOutputReplayKey,
        call_id: &str,
        size_bytes: usize,
        sha256: &str,
    ) {
        let Ok(mut guard) = self.tool_output_replay_seen.lock() else {
            return;
        };
        guard.entry(key).or_insert(ToolOutputReplayServed {
            call_id: call_id.to_string(),
            size_bytes,
            sha256_short: sha256.chars().take(12).collect(),
        });
    }
}
