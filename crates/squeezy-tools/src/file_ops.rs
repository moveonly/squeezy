use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use ignore::WalkBuilder;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use squeezy_vcs::{DiffMode, DiffOptions};
use squeezy_workspace::ExclusionReason;
use tokio_util::sync::CancellationToken;

use crate::{
    DEFAULT_MAX_BYTES_PER_FILE, DEFAULT_MAX_FILES, DEFAULT_READ_LIMIT, MAX_READ_LIMIT,
    POLICY_PREFIX_BYTES, ToolCall, ToolCostHint, ToolRegistry, ToolResult, ToolStatus,
    build_include_set, build_required_glob, diff_path_set, file_len, is_secret_path, make_result,
    read_prefix, read_range, sha256_file, tool_arg_error, tool_error, truncate_text,
    workspace_path,
};

pub(crate) const DEFAULT_MAX_MATCHES: usize = 100;
pub(crate) const DEFAULT_OUTPUT_BYTE_CAP: usize = 24_000;

#[derive(Debug, Deserialize)]
pub(crate) struct GlobArgs {
    pub(crate) pattern: String,
    pub(crate) path: Option<String>,
    include_ignored: Option<bool>,
    diff_only: Option<bool>,
    max_paths: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GrepArgs {
    pub(crate) pattern: String,
    pub(crate) path: Option<String>,
    include: Option<Vec<String>>,
    include_ignored: Option<bool>,
    diff_only: Option<bool>,
    output_mode: Option<GrepOutputMode>,
    max_files: Option<usize>,
    max_bytes_per_file: Option<usize>,
    max_matches: Option<usize>,
    output_byte_cap: Option<usize>,
    offset: Option<usize>,
}

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
pub(crate) struct ReadFileArgs {
    pub(crate) path: String,
    offset: Option<usize>,
    limit: Option<usize>,
    diff_only: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReadToolOutputArgs {
    pub(crate) handle: String,
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
                        let line = truncate_text(line, 500);
                        let next = json!({
                            "path": &rel_str,
                            "line": line_index + 1,
                            "text": line,
                        });
                        let next_len = serde_json::to_string(&next).map_or(0, |text| text.len());
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
        metadata.insert("include_ignored".to_string(), json!(include_ignored));
        metadata.insert("diff_only".to_string(), json!(diff_only));
        metadata.insert("output_mode".to_string(), json!(output_mode.as_str()));
        metadata.insert("offset".to_string(), json!(offset));
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

    pub(crate) async fn execute_read_file(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<ReadFileArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let path = match self.resolve_existing(&args.path) {
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
        let content = String::from_utf8_lossy(&bytes).to_string();
        let cost = ToolCostHint {
            bytes_read: total_bytes,
            output_bytes: content.len() as u64,
            truncated: end < total_bytes as usize,
            ..ToolCostHint::default()
        };

        let mut payload = serde_json::Map::new();
        payload.insert("path".to_string(), json!(&rel_str));
        payload.insert("offset".to_string(), json!(offset));
        payload.insert("bytes_returned".to_string(), json!(bytes.len()));
        payload.insert("total_bytes".to_string(), json!(total_bytes));
        payload.insert("sha256".to_string(), json!(content_sha256));
        payload.insert("truncated".to_string(), json!(end < total_bytes as usize));
        if let Some(reason) = ignored_reason {
            // Keep this opt-in: most reads are not from ignored paths, so
            // skipping these fields shaves two keys off the common case.
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
        let output = match self.output_store.read(
            &args.handle,
            args.offset.unwrap_or(0),
            args.limit.unwrap_or(DEFAULT_READ_LIMIT).min(MAX_READ_LIMIT),
        ) {
            Ok(output) => output,
            Err(err) => return tool_error(call, err),
        };
        let cost = ToolCostHint {
            bytes_read: output.bytes_returned as u64,
            output_bytes: output.content.len() as u64,
            truncated: output.truncated,
            ..ToolCostHint::default()
        };

        make_result(
            call,
            ToolStatus::Success,
            json!({
                "handle": args.handle,
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
}
