//! Stream-friendly extractor for `<proposed_plan>...</proposed_plan>` blocks.
//!
//! Plan-mode replies (see `crates/squeezy-agent/src/plan_mode.rs`) end with a
//! block of the form `<proposed_plan>...</proposed_plan>`. We strip those
//! blocks out of the live assistant transcript and surface them as distinct
//! log entries so the user can see the final plan at a glance even when
//! the surrounding narration is long.
//!
//! Deltas arrive in arbitrary chunks (e.g. mid-tag splits), so the parser
//! buffers across calls. Each call to [`feed`] returns the bytes that
//! should still flow into the live assistant pane, plus any fully closed
//! plan blocks that should be promoted to log entries.

use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) const OPEN_TAG: &str = "<proposed_plan>";
pub(crate) const CLOSE_TAG: &str = "</proposed_plan>";

/// Workspace-relative directory where proposed plans are persisted as
/// markdown. Each plan body lives at `<dir>/<plan_id>.md`. Persisting
/// per-plan-id avoids collisions when multiple sessions run against the
/// same workspace and lets the agent iterate on a specific plan rather
/// than overwriting whatever happened to be there.
pub(crate) const PLAN_DIR: &str = ".squeezy/plans";

/// Stable, body-derived identifier so an unchanged plan reuses its file
/// and a refined plan lands at a new path. 12 hex chars is plenty for
/// per-workspace uniqueness; the `plan-` prefix matches the convention
/// the `plan_patch` tool already uses elsewhere in the codebase.
pub(crate) fn plan_id_for(body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.trim().as_bytes());
    let digest = hasher.finalize();
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("plan-{}", &hex[..12])
}

pub(crate) fn plan_file_for(workspace_root: &Path, plan_id: &str) -> PathBuf {
    workspace_root.join(PLAN_DIR).join(format!("{plan_id}.md"))
}

/// Maximum number of plan files kept under `.squeezy/plans/`. Pruning
/// runs at session start so the directory cannot grow unbounded across
/// many sessions; the cap is high enough that recent plans always
/// survive a normal day of work.
pub(crate) const PLAN_RETENTION_LIMIT: usize = 20;

/// Trim `.squeezy/plans/` to at most [`PLAN_RETENTION_LIMIT`] markdown
/// files, keeping the newest by mtime. Returns the number of files
/// deleted; `0` when the dir is missing, empty, or already under the
/// limit. Read errors are silently treated as "nothing to prune" so a
/// permissions issue can never crash session startup.
pub(crate) fn prune_plan_dir(workspace_root: &Path) -> usize {
    let dir = workspace_root.join(PLAN_DIR);
    let Ok(entries) = fs::read_dir(&dir) else {
        return 0;
    };
    let mut plans: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                return None;
            }
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()?;
            Some((modified, path))
        })
        .collect();
    if plans.len() <= PLAN_RETENTION_LIMIT {
        return 0;
    }
    plans.sort_by_key(|(stem, _)| std::cmp::Reverse(*stem));
    let mut deleted = 0;
    for (_, path) in plans.into_iter().skip(PLAN_RETENTION_LIMIT) {
        if fs::remove_file(&path).is_ok() {
            deleted += 1;
        }
    }
    deleted
}

/// Persist a proposed plan body as `<workspace>/.squeezy/plans/<plan_id>.md`.
/// Returns the plan id and the absolute path. The body is written verbatim
/// (no front-matter) with a trailing newline so the file round-trips
/// cleanly through editors and through `read_file` / `apply_patch`.
pub(crate) fn persist_plan(workspace_root: &Path, body: &str) -> io::Result<(String, PathBuf)> {
    let plan_id = plan_id_for(body);
    let path = plan_file_for(workspace_root, &plan_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut contents = body.trim_end().to_string();
    contents.push('\n');
    fs::write(&path, contents)?;
    Ok((plan_id, path))
}

#[derive(Debug, Default)]
pub(crate) struct ProposedPlanExtractor {
    /// Bytes accumulated inside an unclosed `<proposed_plan>` block.
    inside: Option<String>,
    /// Bytes that *might* be a partial open/close tag straddling a delta
    /// boundary. Always equal to a strict prefix of `OPEN_TAG` or
    /// `CLOSE_TAG` (whichever the current state expects).
    pending_tag: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ProposedPlanFeed {
    /// Bytes that should be appended to the live assistant text.
    pub passthrough: String,
    /// Fully-extracted plan bodies (without surrounding tags) closed by
    /// this delta.
    pub completed: Vec<String>,
}

impl ProposedPlanExtractor {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// True when a `<proposed_plan>` block is currently open (open tag seen,
    /// close tag not yet seen).
    #[cfg(test)]
    pub(crate) fn is_open(&self) -> bool {
        self.inside.is_some()
    }

    /// Feed a streaming delta. The returned `passthrough` should be appended
    /// to the live assistant buffer; `completed` holds finalised plan
    /// bodies (without tags), already trimmed of leading/trailing newlines.
    pub(crate) fn feed(&mut self, delta: &str) -> ProposedPlanFeed {
        let mut out = ProposedPlanFeed::default();
        let mut remaining = delta;
        while !remaining.is_empty() {
            if self.inside.is_some() {
                // Looking for CLOSE_TAG; pending_tag holds a possible partial.
                let mut buf = std::mem::take(&mut self.pending_tag);
                buf.push_str(remaining);
                match buf.find(CLOSE_TAG) {
                    Some(idx) => {
                        let body = &buf[..idx];
                        let inside = self.inside.as_mut().expect("inside guarded above");
                        inside.push_str(body);
                        let completed = std::mem::take(inside).trim().to_string();
                        out.completed.push(completed);
                        self.inside = None;
                        // Resume scanning after the close tag.
                        remaining = remaining_after_match(remaining, &buf, idx + CLOSE_TAG.len());
                    }
                    None => {
                        let safe_len = safe_emit_len(&buf, CLOSE_TAG);
                        let inside = self.inside.as_mut().expect("inside guarded above");
                        inside.push_str(&buf[..safe_len]);
                        self.pending_tag = buf[safe_len..].to_string();
                        remaining = "";
                    }
                }
            } else {
                // Outside any block; looking for OPEN_TAG.
                let mut buf = std::mem::take(&mut self.pending_tag);
                buf.push_str(remaining);
                match buf.find(OPEN_TAG) {
                    Some(idx) => {
                        out.passthrough.push_str(&buf[..idx]);
                        self.inside = Some(String::new());
                        remaining = remaining_after_match(remaining, &buf, idx + OPEN_TAG.len());
                    }
                    None => {
                        let safe_len = safe_emit_len(&buf, OPEN_TAG);
                        out.passthrough.push_str(&buf[..safe_len]);
                        self.pending_tag = buf[safe_len..].to_string();
                        remaining = "";
                    }
                }
            }
        }
        out
    }

    /// Flush any unterminated state. Called when the turn ends.
    /// Returns any text that should still flow into the assistant pane
    /// (an unterminated open tag is treated as plain text — better to
    /// show garbled markers than to silently drop the trailing narration).
    pub(crate) fn finalize(&mut self) -> String {
        // If we are inside an unterminated block, drop it — the audit's
        // contract says exactly one block per turn, so a missing close
        // tag is a model bug. Surface the open marker so a user can spot
        // the issue rather than getting silence.
        let mut leftover = String::new();
        if self.inside.is_some() {
            leftover.push_str(OPEN_TAG);
            let body = self.inside.take().expect("guarded");
            leftover.push_str(&body);
        }
        leftover.push_str(&std::mem::take(&mut self.pending_tag));
        leftover
    }
}

/// Number of bytes from the head of `buf` that can safely flow downstream
/// without losing a partial occurrence of `needle` straddling the
/// boundary. We keep up to `needle.len() - 1` bytes of trailing data in
/// `pending_tag`, but only if those bytes actually match a non-empty
/// prefix of `needle`. This avoids growing `pending_tag` unboundedly when
/// a delta ends with characters that simply happen to overlap part of
/// `needle`.
fn safe_emit_len(buf: &str, needle: &str) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let max_keep = needle.len().saturating_sub(1);
    let mut keep = buf.len().min(max_keep);
    while keep > 0 {
        // `buf.len() - keep` must land on a char boundary for slicing to
        // be safe.
        if buf.is_char_boundary(buf.len() - keep) {
            let tail = &buf[buf.len() - keep..];
            if needle.starts_with(tail) {
                break;
            }
        }
        keep -= 1;
    }
    buf.len() - keep
}

/// Given the original `delta`, the combined `buf` (pending_tag + delta),
/// and the index *into `buf`* just past a match, return the slice of
/// `delta` that follows the match. When the match lay entirely inside
/// the previously-buffered `pending_tag`, this returns the whole `delta`
/// because no bytes of `delta` were consumed yet.
fn remaining_after_match<'a>(delta: &'a str, buf: &str, end_in_buf: usize) -> &'a str {
    let prefix_len = buf.len() - delta.len();
    if end_in_buf <= prefix_len {
        delta
    } else {
        &delta[end_in_buf - prefix_len..]
    }
}

#[cfg(test)]
#[path = "proposed_plan_tests.rs"]
mod tests;
