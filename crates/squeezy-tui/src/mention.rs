//! `@<path>` file mention popup for the composer.
//!
//! Detects an `@`-prefixed token at the cursor and lists matching
//! workspace files ranked by a simple subsequence/prefix scorer. The
//! popup is small (10 entries) and dismisses on Esc or when the
//! cursor leaves the `@<word>` token.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

const MAX_MATCHES: usize = 10;
const MAX_WORKSPACE_FILES: usize = 5000;

/// The state of an in-flight `@`-mention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MentionQuery {
    /// Byte offset of the `@` in the input string.
    pub start: usize,
    /// Byte offset just past the current word.
    pub end: usize,
    /// The token after `@` (may be empty just after typing `@`).
    pub query: String,
}

/// Returns `Some(MentionQuery)` if the cursor sits inside an `@<word>`
/// token preceded by start-of-input or whitespace. `None` otherwise.
pub(crate) fn detect_mention(input: &str, cursor: usize) -> Option<MentionQuery> {
    let bytes = input.as_bytes();
    let cursor = cursor.min(input.len());
    // Walk left from cursor until whitespace or '@'.
    let mut i = cursor;
    while i > 0 {
        let b = bytes[i - 1];
        if b == b'@' {
            // Found the marker. Verify the byte before is whitespace or absent.
            if i >= 2 {
                let prev = bytes[i - 2];
                if !prev.is_ascii_whitespace() {
                    return None;
                }
            }
            // Collect the token from `@` (exclusive) to next whitespace right.
            let start = i - 1;
            let mut end = cursor;
            while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
                end += 1;
            }
            let query = input[i..end].to_string();
            // Reject if there's an embedded whitespace before cursor (shouldn't happen).
            if query.contains(char::is_whitespace) {
                return None;
            }
            return Some(MentionQuery { start, end, query });
        }
        if b.is_ascii_whitespace() {
            return None;
        }
        i -= 1;
    }
    None
}

/// Walk the workspace rooted at `root` (respecting .gitignore via the
/// `ignore` crate) and return up to `MAX_WORKSPACE_FILES` file paths
/// relative to `root`. Used as the candidate set for the popup.
pub(crate) fn load_workspace_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .build();
    for entry in walker {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        files.push(rel);
        if files.len() >= MAX_WORKSPACE_FILES {
            break;
        }
    }
    files
}

/// Rank `files` against `query`. Returns up to `MAX_MATCHES` paths.
///
/// Scoring (higher is better):
///   * 1000 — file name starts with query
///   * 800  — any path component starts with query
///   * 600  — path contains query as a substring
///   * 400  — query characters appear as a subsequence
///   * 0    — no match
pub(crate) fn rank_files(query: &str, files: &[PathBuf]) -> Vec<PathBuf> {
    let query = query.to_ascii_lowercase();
    let mut scored: Vec<(i32, &Path)> = files
        .iter()
        .filter_map(|path| {
            let score = score_path(&query, path);
            if score > 0 {
                Some((score, path.as_path()))
            } else {
                None
            }
        })
        .collect();
    // Higher score first; on tie shorter path first (likely more relevant).
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.as_os_str().len().cmp(&b.1.as_os_str().len()))
    });
    scored
        .into_iter()
        .take(MAX_MATCHES)
        .map(|(_, p)| p.to_path_buf())
        .collect()
}

fn score_path(query: &str, path: &Path) -> i32 {
    if query.is_empty() {
        return 1;
    }
    let display = path.to_string_lossy().to_ascii_lowercase();
    if let Some(name) = path.file_name().and_then(|n| n.to_str())
        && name.to_ascii_lowercase().starts_with(query)
    {
        return 1000;
    }
    for component in path.components() {
        if let Some(c) = component.as_os_str().to_str()
            && c.to_ascii_lowercase().starts_with(query)
        {
            return 800;
        }
    }
    if display.contains(query) {
        return 600;
    }
    if is_subsequence(query, &display) {
        return 400;
    }
    0
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut iter = haystack.chars();
    'outer: for ch in needle.chars() {
        for hc in iter.by_ref() {
            if hc == ch {
                continue 'outer;
            }
        }
        return false;
    }
    true
}

/// Popup state attached to the app.
#[derive(Debug, Clone, Default)]
pub(crate) struct MentionPopup {
    pub query: String,
    pub start: usize,
    pub end: usize,
    pub matches: Vec<PathBuf>,
    pub selected: usize,
}

impl MentionPopup {
    pub(crate) fn from_query(q: MentionQuery, matches: Vec<PathBuf>) -> Self {
        Self {
            query: q.query,
            start: q.start,
            end: q.end,
            matches,
            selected: 0,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    pub(crate) fn selected_path(&self) -> Option<&Path> {
        self.matches.get(self.selected).map(|p| p.as_path())
    }

    pub(crate) fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub(crate) fn move_down(&mut self) {
        if self.selected + 1 < self.matches.len() {
            self.selected += 1;
        }
    }

    /// Returns the (new_input, new_cursor) after inserting the selected
    /// path into `input` at the mention range.
    pub(crate) fn apply(&self, input: &str) -> Option<(String, usize)> {
        let path = self.selected_path()?;
        let replacement = format!("{} ", path.display());
        let mut out = String::with_capacity(input.len() + replacement.len());
        out.push_str(&input[..self.start]);
        out.push_str(&replacement);
        out.push_str(&input[self.end..]);
        let new_cursor = self.start + replacement.len();
        Some((out, new_cursor))
    }
}

#[cfg(test)]
#[path = "mention_tests.rs"]
mod tests;
