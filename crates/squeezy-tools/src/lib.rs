use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use squeezy_core::{PermissionScope, Result, SqueezyError};
use tokio::{process::Command, time};
use tokio_util::sync::CancellationToken;

const DEFAULT_MAX_FILES: usize = 10_000;
const DEFAULT_MAX_BYTES_PER_FILE: usize = 1_000_000;
const DEFAULT_MAX_MATCHES: usize = 100;
const DEFAULT_OUTPUT_BYTE_CAP: usize = 24_000;
const DEFAULT_READ_LIMIT: usize = 32_000;
const MAX_READ_LIMIT: usize = 128_000;
const DEFAULT_SPILL_THRESHOLD_BYTES: usize = 25_000;
const DEFAULT_SPILL_PREVIEW_BYTES: usize = 2_000;
const DEFAULT_TOOL_OUTPUT_RETENTION_DAYS: u64 = 7;
const DEFAULT_SHELL_TIMEOUT_MS: u64 = 30_000;
const MAX_SHELL_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_SHELL_OUTPUT_BYTE_CAP: usize = 32_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolStatus {
    Success,
    Error,
    Denied,
    Stale,
    Cancelled,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCostHint {
    pub files_scanned: u64,
    pub bytes_read: u64,
    pub matches_returned: u64,
    pub output_bytes: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolReceipt {
    pub output_sha256: String,
    pub content_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub tool_name: String,
    pub status: ToolStatus,
    pub content: Value,
    pub cost_hint: ToolCostHint,
    pub receipt: ToolReceipt,
}

impl ToolResult {
    pub fn model_output(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            json!({
                "call_id": self.call_id,
                "tool_name": self.tool_name,
                "status": "error",
                "content": {"error": "tool result serialization failed"},
            })
            .to_string()
        })
    }

    pub fn denied(call: &ToolCall, reason: impl Into<String>) -> Self {
        make_result(
            call,
            ToolStatus::Denied,
            json!({ "error": reason.into() }),
            ToolCostHint::default(),
            None,
        )
    }

    pub fn cancelled(call: &ToolCall) -> Self {
        make_result(
            call,
            ToolStatus::Cancelled,
            json!({ "error": "tool call cancelled" }),
            ToolCostHint::default(),
            None,
        )
    }
}

#[derive(Debug, Clone)]
pub struct ToolRegistry {
    root: Arc<PathBuf>,
    output_store: Arc<ToolOutputStore>,
}

impl ToolRegistry {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let root = root
            .canonicalize()
            .map_err(|err| SqueezyError::Tool(format!("invalid workspace root: {err}")))?;
        let output_store = ToolOutputStore::new(&root)?;
        Ok(Self {
            root: Arc::new(root),
            output_store: Arc::new(output_store),
        })
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = vec![
            glob_spec(),
            grep_spec(),
            read_file_spec(),
            read_tool_output_spec(),
            write_file_spec(),
            shell_spec(),
        ];
        specs.sort_by(|left, right| left.name.cmp(&right.name));
        specs
    }

    pub fn permission_scope(&self, call: &ToolCall) -> PermissionScope {
        match call.name.as_str() {
            "write_file" => PermissionScope::Edit,
            "shell" => PermissionScope::Shell,
            "glob" if tool_include_ignored(&call.arguments) => PermissionScope::IgnoredSearch,
            "grep" if grep_include_ignored(&call.arguments) => PermissionScope::IgnoredSearch,
            "glob" | "grep" | "read_file" | "read_tool_output" => PermissionScope::Read,
            _ => PermissionScope::Read,
        }
    }

    pub fn is_parallel_safe(&self, call: &ToolCall) -> bool {
        matches!(
            call.name.as_str(),
            "glob" | "grep" | "read_file" | "read_tool_output"
        )
    }

    pub fn describe_call(&self, call: &ToolCall) -> String {
        match call.name.as_str() {
            "glob" => {
                let args = serde_json::from_value::<GlobArgs>(call.arguments.clone()).ok();
                let pattern = args
                    .as_ref()
                    .map(|args| args.pattern.as_str())
                    .unwrap_or("?");
                let path = args
                    .as_ref()
                    .and_then(|args| args.path.as_deref())
                    .unwrap_or(".");
                format!("glob pattern={pattern:?} path={path:?}")
            }
            "grep" => {
                let args = serde_json::from_value::<GrepArgs>(call.arguments.clone()).ok();
                let pattern = args
                    .as_ref()
                    .map(|args| args.pattern.as_str())
                    .unwrap_or("?");
                let path = args
                    .as_ref()
                    .and_then(|args| args.path.as_deref())
                    .unwrap_or(".");
                format!("grep pattern={pattern:?} path={path:?}")
            }
            "read_file" => {
                let args = serde_json::from_value::<ReadFileArgs>(call.arguments.clone()).ok();
                let path = args.as_ref().map(|args| args.path.as_str()).unwrap_or("?");
                format!("read_file path={path:?}")
            }
            "read_tool_output" => {
                let args =
                    serde_json::from_value::<ReadToolOutputArgs>(call.arguments.clone()).ok();
                let handle = args
                    .as_ref()
                    .map(|args| args.handle.as_str())
                    .unwrap_or("?");
                format!("read_tool_output handle={handle:?}")
            }
            "write_file" => {
                let args = serde_json::from_value::<WriteFileArgs>(call.arguments.clone()).ok();
                let path = args.as_ref().map(|args| args.path.as_str()).unwrap_or("?");
                format!("write_file path={path:?}")
            }
            "shell" => {
                let args = serde_json::from_value::<ShellArgs>(call.arguments.clone()).ok();
                let description = args
                    .as_ref()
                    .and_then(|args| args.description.as_deref())
                    .unwrap_or("run shell command");
                format!("shell {description}")
            }
            _ => format!("{} {}", call.name, call.arguments),
        }
    }

    pub async fn execute(&self, call: ToolCall, cancel: CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return ToolResult::cancelled(&call);
        }

        let result = match call.name.as_str() {
            "glob" => self.execute_glob(&call, cancel).await,
            "grep" => self.execute_grep(&call, cancel).await,
            "read_file" => self.execute_read_file(&call).await,
            "read_tool_output" => self.execute_read_tool_output(&call).await,
            "write_file" => self.execute_write_file(&call).await,
            "shell" => self.execute_shell(&call, cancel).await,
            _ => make_result(
                &call,
                ToolStatus::Error,
                json!({ "error": format!("unknown tool: {}", call.name) }),
                ToolCostHint::default(),
                None,
            ),
        };

        if call.name == "read_tool_output" {
            result
        } else {
            self.finalize_result(result)
        }
    }

    async fn execute_glob(&self, call: &ToolCall, cancel: CancellationToken) -> ToolResult {
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
            if is_secret_path(path) {
                skipped_secret_files += 1;
                continue;
            }
            cost.files_scanned += 1;
            let rel = self.relative(path);
            if !pattern.is_match(rel.as_path()) {
                continue;
            }
            if skipped_paths < offset {
                skipped_paths += 1;
                continue;
            }
            paths.push(json!(rel.to_string_lossy()));
            cost.matches_returned += 1;
        }

        make_result(
            call,
            ToolStatus::Success,
            json!({
                "paths": paths,
                "metadata": {
                    "include_ignored": include_ignored,
                    "offset": offset,
                    "skipped_secret_files": skipped_secret_files,
                },
            }),
            cost,
            None,
        )
    }

    async fn execute_grep(&self, call: &ToolCall, cancel: CancellationToken) -> ToolResult {
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

        for entry in builder.build() {
            if cancel.is_cancelled() {
                return ToolResult::cancelled(call);
            }
            if scanned_files >= max_files
                || output_mode.is_limited(matches.len(), paths.len(), max_matches)
                || cost.truncated
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
            if include
                .as_ref()
                .is_some_and(|include| !include.is_match(rel.as_path()))
            {
                continue;
            }
            if is_secret_path(path) {
                skipped_secret_files += 1;
                continue;
            }

            scanned_files += 1;
            cost.files_scanned += 1;
            let bytes = match read_prefix(path, max_bytes_per_file) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
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
                            "path": rel.to_string_lossy(),
                            "line": line_index + 1,
                            "text": line,
                        });
                        let next_len = serde_json::to_string(&next).map_or(0, |text| text.len());
                        if cost.output_bytes + next_len as u64 > output_byte_cap as u64 {
                            cost.truncated = true;
                            break;
                        }
                        cost.output_bytes += next_len as u64;
                        cost.matches_returned += 1;
                        matches.push(next);
                    }
                    GrepOutputMode::FilesWithMatches => {
                        if paths.insert(rel.to_string_lossy().to_string()) {
                            cost.matches_returned += 1;
                        }
                    }
                    GrepOutputMode::Count => {
                        cost.matches_returned = count;
                    }
                }
                if output_mode.is_limited(matches.len(), paths.len(), max_matches) {
                    cost.truncated = true;
                    break;
                }
            }
        }

        let mut metadata = BTreeMap::new();
        metadata.insert("include_ignored".to_string(), json!(include_ignored));
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

    async fn execute_read_file(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<ReadFileArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let path = match self.resolve_existing(&args.path) {
            Ok(path) => path,
            Err(err) => return tool_error(call, err),
        };
        if is_secret_path(&path) {
            return make_result(
                call,
                ToolStatus::Denied,
                json!({ "error": "refusing to read a likely secret file" }),
                ToolCostHint::default(),
                None,
            );
        }

        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => return tool_error(call, err),
        };
        let offset = args.offset.unwrap_or(0).min(bytes.len());
        let limit = args.limit.unwrap_or(DEFAULT_READ_LIMIT).min(MAX_READ_LIMIT);
        let end = offset.saturating_add(limit).min(bytes.len());
        let content = String::from_utf8_lossy(&bytes[offset..end]).to_string();
        let content_sha256 = sha256_hex(&bytes);
        let cost = ToolCostHint {
            bytes_read: (end - offset) as u64,
            output_bytes: content.len() as u64,
            truncated: end < bytes.len(),
            ..ToolCostHint::default()
        };

        make_result(
            call,
            ToolStatus::Success,
            json!({
                "path": self.relative(&path).to_string_lossy(),
                "offset": offset,
                "bytes_returned": end - offset,
                "total_bytes": bytes.len(),
                "sha256": content_sha256,
                "truncated": end < bytes.len(),
                "content": content,
            }),
            cost,
            Some(content_sha256),
        )
    }

    async fn execute_read_tool_output(&self, call: &ToolCall) -> ToolResult {
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

    async fn execute_write_file(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<WriteFileArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let path = match self.resolve_for_write(&args.path) {
            Ok(path) => path,
            Err(err) => return tool_error(call, err),
        };
        if is_secret_path(&path) {
            return make_result(
                call,
                ToolStatus::Denied,
                json!({ "error": "refusing to write a likely secret file" }),
                ToolCostHint::default(),
                None,
            );
        }

        let before = fs::read(&path).ok();
        let before_sha256 = before.as_ref().map(sha256_hex);
        if before.is_some() && args.expected_sha256.as_deref() != before_sha256.as_deref() {
            return make_result(
                call,
                ToolStatus::Stale,
                json!({
                    "error": "expected_sha256 does not match current file",
                    "path": self.relative(&path).to_string_lossy(),
                    "current_sha256": before_sha256,
                }),
                ToolCostHint::default(),
                before_sha256,
            );
        }

        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            return tool_error(call, err);
        }
        if let Err(err) = fs::write(&path, args.content.as_bytes()) {
            return tool_error(call, err);
        }

        let after_sha256 = sha256_hex(args.content.as_bytes());
        let cost = ToolCostHint {
            bytes_read: before.as_ref().map_or(0, |bytes| bytes.len() as u64),
            output_bytes: args.content.len() as u64,
            ..ToolCostHint::default()
        };

        make_result(
            call,
            ToolStatus::Success,
            json!({
                "path": self.relative(&path).to_string_lossy(),
                "before_sha256": before_sha256,
                "after_sha256": after_sha256,
                "bytes_written": args.content.len(),
            }),
            cost,
            Some(after_sha256),
        )
    }

    async fn execute_shell(&self, call: &ToolCall, cancel: CancellationToken) -> ToolResult {
        let args = match serde_json::from_value::<ShellArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let workdir = match self.resolve_existing(args.workdir.as_deref().unwrap_or(".")) {
            Ok(path) => path,
            Err(err) => return tool_error(call, err),
        };
        let timeout_ms = args
            .timeout_ms
            .unwrap_or(DEFAULT_SHELL_TIMEOUT_MS)
            .min(MAX_SHELL_TIMEOUT_MS);
        let output_cap = args
            .output_byte_cap
            .unwrap_or(DEFAULT_SHELL_OUTPUT_BYTE_CAP)
            .min(128_000);

        let mut command = Command::new("sh");
        command
            .arg("-lc")
            .arg(&args.command)
            .current_dir(&workdir)
            .kill_on_drop(true);
        let output = command.output();

        let output = tokio::select! {
            _ = cancel.cancelled() => return ToolResult::cancelled(call),
            result = time::timeout(Duration::from_millis(timeout_ms), output) => result,
        };

        let output = match output {
            Ok(Ok(output)) => output,
            Ok(Err(err)) => return tool_error(call, err),
            Err(_) => {
                return make_result(
                    call,
                    ToolStatus::Error,
                    json!({ "error": format!("shell command timed out after {timeout_ms} ms") }),
                    ToolCostHint {
                        truncated: true,
                        ..ToolCostHint::default()
                    },
                    None,
                );
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let (stdout, stdout_truncated) = truncate_to_bytes(stdout.as_ref(), output_cap);
        let remaining = output_cap.saturating_sub(stdout.len());
        let (stderr, stderr_truncated) = truncate_to_bytes(stderr.as_ref(), remaining);
        let truncated = stdout_truncated || stderr_truncated;
        let cost = ToolCostHint {
            output_bytes: (stdout.len() + stderr.len()) as u64,
            truncated,
            ..ToolCostHint::default()
        };
        let status = if output.status.success() {
            ToolStatus::Success
        } else {
            ToolStatus::Error
        };

        make_result(
            call,
            status,
            json!({
                "command": args.command,
                "workdir": self.relative(&workdir).to_string_lossy(),
                "exit_code": output.status.code(),
                "stdout": stdout,
                "stderr": stderr,
                "truncated": truncated,
            }),
            cost,
            None,
        )
    }

    fn resolve_existing(&self, raw: &str) -> std::result::Result<PathBuf, String> {
        let candidate = self.join_workspace(raw)?;
        let canonical = candidate
            .canonicalize()
            .map_err(|err| format!("path does not exist or is inaccessible: {err}"))?;
        self.ensure_inside(canonical)
    }

    fn resolve_for_write(&self, raw: &str) -> std::result::Result<PathBuf, String> {
        let candidate = self.join_workspace(raw)?;
        if candidate.exists() {
            return self.resolve_existing(raw);
        }
        let parent = candidate
            .parent()
            .ok_or_else(|| "path has no parent".to_string())?;
        let parent = parent
            .canonicalize()
            .map_err(|err| format!("parent directory does not exist or is inaccessible: {err}"))?;
        self.ensure_inside(parent)?;
        Ok(candidate)
    }

    fn join_workspace(&self, raw: &str) -> std::result::Result<PathBuf, String> {
        if raw.trim().is_empty() {
            return Err("path must not be empty".to_string());
        }
        let path = Path::new(raw);
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err("path must stay inside the workspace".to_string());
        }
        Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        })
    }

    fn ensure_inside(&self, canonical: PathBuf) -> std::result::Result<PathBuf, String> {
        if canonical.starts_with(self.root.as_ref()) {
            Ok(canonical)
        } else {
            Err("path is outside the workspace".to_string())
        }
    }

    fn relative(&self, path: &Path) -> PathBuf {
        path.strip_prefix(self.root.as_ref())
            .unwrap_or(path)
            .to_path_buf()
    }

    fn finalize_result(&self, result: ToolResult) -> ToolResult {
        self.output_store.maybe_spill(result)
    }
}

#[derive(Debug, Clone)]
struct ToolOutputStore {
    dir: PathBuf,
    spill_threshold_bytes: usize,
    preview_bytes: usize,
    retention: Duration,
}

impl ToolOutputStore {
    fn new(root: &Path) -> Result<Self> {
        let store = Self {
            dir: root.join(".squeezy").join("tool_outputs"),
            spill_threshold_bytes: DEFAULT_SPILL_THRESHOLD_BYTES,
            preview_bytes: DEFAULT_SPILL_PREVIEW_BYTES,
            retention: Duration::from_secs(DEFAULT_TOOL_OUTPUT_RETENTION_DAYS * 24 * 60 * 60),
        };
        fs::create_dir_all(&store.dir)?;
        store.cleanup_old_outputs();
        Ok(store)
    }

    fn maybe_spill(&self, mut result: ToolResult) -> ToolResult {
        let output = result.model_output();
        if output.len() <= self.spill_threshold_bytes {
            return result;
        }

        let sha256 = sha256_hex(output.as_bytes());
        let path = self.path_for(&sha256);
        if let Err(err) = fs::write(&path, output.as_bytes()) {
            result.status = ToolStatus::Error;
            result.content = json!({ "error": format!("failed to spill tool output: {err}") });
            result.cost_hint.truncated = true;
            result.receipt.output_sha256 =
                sha256_hex(serde_json::to_vec(&result.content).unwrap_or_default());
            return result;
        }

        let (preview, _) = truncate_to_bytes(&output, self.preview_bytes);
        let ToolResult {
            call_id,
            tool_name,
            status,
            content: _,
            mut cost_hint,
            receipt,
        } = result;
        let call = ToolCall {
            call_id,
            name: tool_name,
            arguments: Value::Null,
        };
        cost_hint.truncated = true;
        cost_hint.output_bytes = 0;

        make_result(
            &call,
            status,
            json!({
                "spilled": true,
                "handle": sha256,
                "sha256": sha256,
                "total_bytes": output.len(),
                "preview_bytes": preview.len(),
                "preview": preview,
                "truncated": true,
            }),
            cost_hint,
            receipt.content_sha256,
        )
    }

    fn read(
        &self,
        handle: &str,
        offset: usize,
        limit: usize,
    ) -> std::result::Result<StoredToolOutputSlice, String> {
        if !is_valid_handle(handle) {
            return Err("invalid tool output handle".to_string());
        }
        let bytes = fs::read(self.path_for(handle))
            .map_err(|err| format!("tool output handle not found or unreadable: {err}"))?;
        let offset = offset.min(bytes.len());
        let end = offset.saturating_add(limit).min(bytes.len());
        let content = String::from_utf8_lossy(&bytes[offset..end]).to_string();
        Ok(StoredToolOutputSlice {
            offset,
            bytes_returned: end - offset,
            total_bytes: bytes.len(),
            sha256: sha256_hex(&bytes),
            truncated: end < bytes.len(),
            content,
        })
    }

    fn cleanup_old_outputs(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            if now
                .duration_since(modified)
                .is_ok_and(|age| age > self.retention)
            {
                let _ = fs::remove_file(path);
            }
        }
    }

    fn path_for(&self, handle: &str) -> PathBuf {
        self.dir.join(format!("{handle}.json"))
    }
}

#[derive(Debug)]
struct StoredToolOutputSlice {
    offset: usize,
    bytes_returned: usize,
    total_bytes: usize,
    sha256: String,
    truncated: bool,
    content: String,
}

fn is_valid_handle(handle: &str) -> bool {
    handle.len() == 64 && handle.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Debug, Deserialize)]
struct GlobArgs {
    pattern: String,
    path: Option<String>,
    include_ignored: Option<bool>,
    max_paths: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    include: Option<Vec<String>>,
    include_ignored: Option<bool>,
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
struct ReadFileArgs {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ReadToolOutputArgs {
    handle: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
    expected_sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ShellArgs {
    command: String,
    workdir: Option<String>,
    timeout_ms: Option<u64>,
    output_byte_cap: Option<usize>,
    description: Option<String>,
}

fn grep_include_ignored(arguments: &Value) -> bool {
    tool_include_ignored(arguments)
}

fn tool_include_ignored(arguments: &Value) -> bool {
    arguments
        .get("include_ignored")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn make_result(
    call: &ToolCall,
    status: ToolStatus,
    content: Value,
    mut cost_hint: ToolCostHint,
    content_sha256: Option<String>,
) -> ToolResult {
    let output = serde_json::to_vec(&content).unwrap_or_default();
    cost_hint.output_bytes = cost_hint.output_bytes.max(output.len() as u64);
    ToolResult {
        call_id: call.call_id.clone(),
        tool_name: call.name.clone(),
        status,
        content,
        cost_hint,
        receipt: ToolReceipt {
            output_sha256: sha256_hex(&output),
            content_sha256,
        },
    }
}

fn tool_arg_error(call: &ToolCall, err: serde_json::Error) -> ToolResult {
    make_result(
        call,
        ToolStatus::Error,
        json!({ "error": format!("invalid tool arguments: {err}") }),
        ToolCostHint::default(),
        None,
    )
}

fn tool_error(call: &ToolCall, err: impl ToString) -> ToolResult {
    make_result(
        call,
        ToolStatus::Error,
        json!({ "error": err.to_string() }),
        ToolCostHint::default(),
        None,
    )
}

fn build_required_glob(pattern: &str) -> std::result::Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    if pattern.contains('/') {
        builder.add(Glob::new(pattern).map_err(|err| err.to_string())?);
    } else {
        builder.add(Glob::new(pattern).map_err(|err| err.to_string())?);
        builder.add(Glob::new(&format!("**/{pattern}")).map_err(|err| err.to_string())?);
    }
    builder.build().map_err(|err| err.to_string())
}

fn build_include_set(patterns: Option<&[String]>) -> std::result::Result<Option<GlobSet>, String> {
    let Some(patterns) = patterns else {
        return Ok(None);
    };
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        if pattern.contains('/') {
            builder.add(Glob::new(pattern).map_err(|err| err.to_string())?);
        } else {
            builder.add(Glob::new(pattern).map_err(|err| err.to_string())?);
            builder.add(Glob::new(&format!("**/{pattern}")).map_err(|err| err.to_string())?);
        }
    }
    builder.build().map(Some).map_err(|err| err.to_string())
}

fn read_prefix(path: &Path, limit: usize) -> std::result::Result<Vec<u8>, std::io::Error> {
    let mut file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.by_ref().take(limit as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn file_len(path: &Path) -> std::result::Result<u64, std::io::Error> {
    Ok(fs::metadata(path)?.len())
}

fn contains_skipped_dir(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|part| matches!(part, ".git" | ".hg" | ".svn" | ".squeezy"))
    })
}

fn is_secret_path(path: &Path) -> bool {
    path.components().any(|component| {
        let Some(part) = component.as_os_str().to_str() else {
            return false;
        };
        let part = part.to_ascii_lowercase();
        part == ".env"
            || part.starts_with(".env.")
            || part.contains("secret")
            || part.contains("credential")
            || part == "id_rsa"
            || part == "id_ed25519"
            || part.ends_with(".pem")
            || part.ends_with(".key")
            || part.ends_with(".p12")
    })
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for ch in value.chars().take(max_chars) {
        output.push(ch);
    }
    if output.len() < value.len() {
        output.push_str("...");
    }
    output
}

fn truncate_to_bytes(value: &str, cap: usize) -> (String, bool) {
    if value.len() <= cap {
        return (value.to_string(), false);
    }
    let mut end = cap;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_string(), true)
}

pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    let digest = Sha256::digest(bytes.as_ref());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn grep_spec() -> ToolSpec {
    ToolSpec {
        name: "grep".to_string(),
        description: "Search text files under a workspace path. Respects .gitignore by default; set include_ignored=true only when ignored files are intentionally needed. Use output_mode=count or files_with_matches for broad exploration before reading content.".to_string(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "pattern": {"type": "string", "description": "Rust regex pattern to search for."},
                "path": {"type": "string", "description": "Workspace-relative file or directory to search.", "default": "."},
                "include": {"type": "array", "items": {"type": "string"}, "description": "Optional glob patterns such as *.rs or crates/**/lib.rs."},
                "include_ignored": {"type": "boolean", "description": "When true, include files ignored by .gitignore and other ignore files. Default false."},
                "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"], "description": "Return matching lines, only files containing matches, or only a count. Default content."},
                "max_files": {"type": "integer", "minimum": 1, "maximum": DEFAULT_MAX_FILES},
                "max_bytes_per_file": {"type": "integer", "minimum": 1, "maximum": DEFAULT_MAX_BYTES_PER_FILE},
                "max_matches": {"type": "integer", "minimum": 1, "maximum": 1000},
                "output_byte_cap": {"type": "integer", "minimum": 1, "maximum": 128000},
                "offset": {"type": "integer", "minimum": 0, "description": "Number of matching lines to skip for pagination."}
            },
            "required": ["pattern"]
        }),
    }
}

fn glob_spec() -> ToolSpec {
    ToolSpec {
        name: "glob".to_string(),
        description: "List workspace file paths matching a glob without reading file contents. Respects .gitignore by default; set include_ignored=true only when ignored paths are intentionally needed.".to_string(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern such as *.rs or crates/**/Cargo.toml."},
                "path": {"type": "string", "description": "Workspace-relative directory to search.", "default": "."},
                "include_ignored": {"type": "boolean", "description": "When true, include files ignored by .gitignore and other ignore files. Default false."},
                "max_paths": {"type": "integer", "minimum": 1, "maximum": 1000},
                "offset": {"type": "integer", "minimum": 0, "description": "Number of matched paths to skip for pagination."}
            },
            "required": ["pattern"]
        }),
    }
}

fn read_file_spec() -> ToolSpec {
    ToolSpec {
        name: "read_file".to_string(),
        description: "Read a bounded byte slice from one workspace file and return its sha256 receipt. Use grep first when locating unknown files.".to_string(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string", "description": "Workspace-relative file path."},
                "offset": {"type": "integer", "minimum": 0, "description": "Byte offset to start reading from."},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_READ_LIMIT, "description": "Maximum bytes to return."}
            },
            "required": ["path"]
        }),
    }
}

fn read_tool_output_spec() -> ToolSpec {
    ToolSpec {
        name: "read_tool_output".to_string(),
        description:
            "Read a bounded byte range from a spilled tool-output handle returned by another tool."
                .to_string(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "handle": {"type": "string", "description": "Tool output handle from a spilled result."},
                "offset": {"type": "integer", "minimum": 0, "description": "Byte offset to start reading from."},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_READ_LIMIT, "description": "Maximum bytes to return."}
            },
            "required": ["handle"]
        }),
    }
}

fn write_file_spec() -> ToolSpec {
    ToolSpec {
        name: "write_file".to_string(),
        description: "Replace a workspace file with exact content. Existing files require expected_sha256 from read_file to prevent stale writes.".to_string(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string", "description": "Workspace-relative file path."},
                "content": {"type": "string", "description": "Full replacement file content."},
                "expected_sha256": {"type": "string", "description": "sha256 of the current file content. Required for existing files."}
            },
            "required": ["path", "content"]
        }),
    }
}

fn shell_spec() -> ToolSpec {
    ToolSpec {
        name: "shell".to_string(),
        description: "Run a bounded shell command in the workspace. Use for verification commands after explaining the purpose in description.".to_string(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "command": {"type": "string", "description": "Command passed to sh -lc."},
                "workdir": {"type": "string", "description": "Workspace-relative working directory.", "default": "."},
                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_SHELL_TIMEOUT_MS},
                "output_byte_cap": {"type": "integer", "minimum": 1, "maximum": 128000},
                "description": {"type": "string", "description": "Short reason this command is needed."}
            },
            "required": ["command", "description"]
        }),
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
