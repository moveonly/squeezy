use std::collections::{BTreeMap, BTreeSet};

use crate::{
    DiffFileStatus, DiffHunk, LargeFileFingerprint, PatchOpKind, PatchOpPreview, RawFileSnapshot,
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct FileStat {
    pub(crate) additions: u64,
    pub(crate) deletions: u64,
    pub(crate) binary: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Patch {
    pub(crate) text: String,
    pub(crate) truncated: bool,
}

pub(crate) fn diff_large_files(
    before: &[LargeFileFingerprint],
    after: &[LargeFileFingerprint],
) -> Vec<String> {
    let before_map: BTreeMap<&str, &LargeFileFingerprint> = before
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let after_map: BTreeMap<&str, &LargeFileFingerprint> = after
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let mut changed = BTreeSet::<String>::new();
    for path in before_map.keys().chain(after_map.keys()) {
        if before_map.get(path) != after_map.get(path) {
            changed.insert((*path).to_string());
        }
    }
    changed.into_iter().collect()
}

pub(crate) fn diff_raw_files(
    before: &BTreeMap<String, RawFileSnapshot>,
    after: &BTreeMap<String, RawFileSnapshot>,
) -> Vec<String> {
    let mut changed = BTreeSet::<String>::new();
    for path in before.keys().chain(after.keys()) {
        if before.get(path) != after.get(path) {
            changed.insert(path.clone());
        }
    }
    changed.into_iter().collect()
}

pub(crate) fn status_kind(code: &str) -> DiffFileStatus {
    if code == "??" || (code.contains('A') && !code.contains('D')) {
        DiffFileStatus::Added
    } else if code.contains('D') && !code.contains('A') {
        DiffFileStatus::Deleted
    } else {
        DiffFileStatus::Modified
    }
}

pub(crate) fn nul_fields(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| String::from_utf8_lossy(field).to_string())
        .collect()
}

pub(crate) fn parse_numstat(bytes: &[u8]) -> BTreeMap<String, FileStat> {
    let mut output = BTreeMap::new();
    let text = String::from_utf8_lossy(bytes);
    for record in text.split('\0').filter(|record| !record.trim().is_empty()) {
        let mut parts = record.splitn(3, '\t');
        let Some(additions) = parts.next() else {
            continue;
        };
        let Some(deletions) = parts.next() else {
            continue;
        };
        let Some(path) = parts.next() else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        let binary = additions == "-" || deletions == "-";
        output.insert(
            path.to_string(),
            FileStat {
                additions: parse_count(additions),
                deletions: parse_count(deletions),
                binary,
            },
        );
    }
    output
}

fn parse_count(value: &str) -> u64 {
    if value == "-" {
        0
    } else {
        value.parse().unwrap_or(0)
    }
}

pub(crate) fn split_unified_patch(
    bytes: &[u8],
    expected_files: &[String],
    max_bytes: usize,
) -> BTreeMap<String, Patch> {
    let text = String::from_utf8_lossy(bytes);
    let expected: BTreeSet<&str> = expected_files.iter().map(String::as_str).collect();
    let mut out: BTreeMap<String, Patch> = BTreeMap::new();
    let mut current_path: Option<String> = None;
    let mut buffer = String::new();
    let flush = |path: Option<String>, buffer: &mut String, out: &mut BTreeMap<String, Patch>| {
        if let Some(path) = path {
            let truncated = buffer.len() > max_bytes;
            let text = if truncated {
                let end = (0..=max_bytes)
                    .rev()
                    .find(|&i| buffer.is_char_boundary(i))
                    .unwrap_or(0);
                buffer[..end].to_string()
            } else {
                std::mem::take(buffer)
            };
            out.insert(path, Patch { text, truncated });
        }
        buffer.clear();
    };
    for line in text.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("diff --git a/")
            && let Some(path) = extract_diff_git_path(rest, &expected)
        {
            flush(current_path.take(), &mut buffer, &mut out);
            current_path = Some(path);
        }
        buffer.push_str(line);
    }
    flush(current_path, &mut buffer, &mut out);
    out
}

fn extract_diff_git_path<'a>(suffix: &'a str, expected: &BTreeSet<&'a str>) -> Option<String> {
    let trimmed = suffix.trim_end_matches('\n').trim_end_matches('\r');
    if let Some(idx) = trimmed.find(" b/") {
        let candidate = &trimmed[..idx];
        if expected.contains(candidate) {
            return Some(candidate.to_string());
        }
    }
    for &path in expected {
        if let Some(prefix) = trimmed.strip_suffix(path)
            && let Some(prefix) = prefix.strip_suffix(" b/")
            && prefix == path
        {
            return Some(path.to_string());
        }
    }
    None
}

pub(crate) fn capped_patch(bytes: Vec<u8>, max_bytes: usize) -> Patch {
    let truncated = bytes.len() > max_bytes;
    let text = if truncated {
        String::from_utf8_lossy(&bytes[..max_bytes]).to_string()
    } else {
        String::from_utf8_lossy(&bytes).to_string()
    };
    Patch { text, truncated }
}

pub fn parse_patch_hunks(patch: &str) -> Vec<DiffHunk> {
    let mut seen = BTreeSet::new();
    let mut hunks = Vec::new();
    for line in patch.lines() {
        let Some(header) = line.strip_prefix("@@ ") else {
            continue;
        };
        let Some(end) = header.find(" @@") else {
            continue;
        };
        let ranges = &header[..end];
        let mut parts = ranges.split_whitespace();
        let old = parts.next().unwrap_or_default().trim_start_matches('-');
        let new = parts.next().unwrap_or_default().trim_start_matches('+');
        let (old_start, old_lines) = parse_hunk_range(old);
        let (new_start, new_lines) = parse_hunk_range(new);
        let start_line = new_start.saturating_sub(1);
        let end_line = if new_lines == 0 {
            start_line
        } else {
            new_start.saturating_add(new_lines).saturating_sub(2)
        };
        let hunk = DiffHunk {
            old_start,
            old_lines,
            new_start,
            new_lines,
            start_line,
            end_line,
        };
        if seen.insert((
            hunk.old_start,
            hunk.new_start,
            hunk.old_lines,
            hunk.new_lines,
        )) {
            hunks.push(hunk);
        }
    }
    hunks
}

fn parse_hunk_range(value: &str) -> (u32, u32) {
    let mut parts = value.split(',');
    let start = parts
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let lines = parts
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    (start, lines)
}

pub(crate) fn op_preview_from_operation(
    index: usize,
    op: &serde_json::Value,
    sha256_hex: impl Fn(&[u8]) -> String,
) -> Option<PatchOpPreview> {
    let kind = op.get("kind").and_then(|kind| kind.as_str())?;
    match kind {
        "search_replace" => {
            let path = op.get("path").and_then(|path| path.as_str())?.to_string();
            Some(PatchOpPreview {
                index,
                kind: PatchOpKind::SearchReplace,
                path,
                from_path: None,
                search_hash: op
                    .get("search")
                    .and_then(|value| value.as_str())
                    .map(|text| sha256_hex(text.as_bytes())),
                replace_hash: op
                    .get("replace")
                    .and_then(|value| value.as_str())
                    .map(|text| sha256_hex(text.as_bytes())),
                contents_hash: None,
            })
        }
        "create_file" => {
            let path = op.get("path").and_then(|path| path.as_str())?.to_string();
            Some(PatchOpPreview {
                index,
                kind: PatchOpKind::CreateFile,
                path,
                from_path: None,
                search_hash: None,
                replace_hash: None,
                contents_hash: op
                    .get("contents")
                    .and_then(|value| value.as_str())
                    .map(|text| sha256_hex(text.as_bytes())),
            })
        }
        "delete_file" => {
            let path = op.get("path").and_then(|path| path.as_str())?.to_string();
            Some(PatchOpPreview {
                index,
                kind: PatchOpKind::DeleteFile,
                path,
                from_path: None,
                search_hash: None,
                replace_hash: None,
                contents_hash: None,
            })
        }
        "move_file" => {
            let from = op.get("from").and_then(|from| from.as_str())?.to_string();
            let to = op.get("to").and_then(|to| to.as_str())?.to_string();
            Some(PatchOpPreview {
                index,
                kind: PatchOpKind::MoveFile,
                path: to,
                from_path: Some(from),
                search_hash: None,
                replace_hash: None,
                contents_hash: None,
            })
        }
        _ => None,
    }
}

pub(crate) fn op_preview_from_search_replace(
    index: usize,
    patch: &serde_json::Value,
    sha256_hex: impl Fn(&[u8]) -> String,
) -> Option<PatchOpPreview> {
    let path = patch
        .get("path")
        .and_then(|path| path.as_str())?
        .to_string();
    Some(PatchOpPreview {
        index,
        kind: PatchOpKind::SearchReplace,
        path,
        from_path: None,
        search_hash: patch
            .get("search")
            .and_then(|value| value.as_str())
            .map(|text| sha256_hex(text.as_bytes())),
        replace_hash: patch
            .get("replace")
            .and_then(|value| value.as_str())
            .map(|text| sha256_hex(text.as_bytes())),
        contents_hash: None,
    })
}
