//! Per-session tempfile spillover for shell-tool output that exceeds
//! the truncation budget.
//!
//! Squeezy hard-caps its in-memory shell stdout/stderr capture at
//! `output_cap` bytes and middle-truncates the result the model sees.
//! When the capture overflows the cap, the bytes past the boundary
//! would otherwise be permanently lost — discarding the signal a long
//! build log, verbose stack trace, or other oversized output carries.
//!
//! [`ShellSpilloverStore`] preserves the captured raw stdout/stderr by
//! writing it to a per-session directory under
//! `$TMPDIR/squeezy-spillover/<session-id>/`. The shell tool surfaces
//! the path in the truncated result so the model can call
//! `read_tool_output { path }` to fetch byte ranges.
//!
//! The store enforces a per-session byte budget (default 100 MB) to
//! bound transient disk usage, and best-effort cleans up its directory
//! on Drop so the spillover never outlives the registry that produced
//! it.

use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::sha256_hex;

/// Per-session byte budget for tempfile spillover. 100 MB matches the
/// cap requested by the F01 finding and keeps transient disk usage
/// bounded even for very long sessions.
pub(crate) const DEFAULT_SHELL_SPILLOVER_BUDGET_BYTES: u64 = 100 * 1024 * 1024;

/// Hash prefix length used in spillover filenames. 16 hex chars is
/// enough to keep collisions astronomically unlikely while keeping the
/// path human-scannable.
const SPILL_SHORT_HASH_HEX: usize = 16;

const STDERR_SEPARATOR: &str = "\n===== stderr =====\n";

/// Global counter that distinguishes session directories created back
/// to back inside the same process. Combined with the PID and
/// monotonic timestamp it makes the session-dir name effectively
/// unforgeable even under heavy concurrent registry construction.
static SESSION_NONCE: AtomicU64 = AtomicU64::new(0);

/// Per-session spillover state. One instance lives in the
/// [`crate::ToolRegistry`] inside an `Arc`; cleanup runs when the last
/// reference drops.
#[derive(Debug)]
pub(crate) struct ShellSpilloverStore {
    session_dir: PathBuf,
    budget_bytes: u64,
    bytes_used: AtomicU64,
}

/// Metadata returned by [`ShellSpilloverStore::spill`] when the
/// captured bytes were durably written to a tempfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellSpilloverInfo {
    pub path: PathBuf,
    pub bytes: u64,
}

/// Byte slice + receipt metadata returned by
/// [`ShellSpilloverStore::read_range`]. Mirrors the shape returned by
/// `read_tool_output` over sha256 handles so the tool dispatch can
/// merge both paths trivially.
#[derive(Debug)]
pub(crate) struct ShellSpilloverReadResult {
    pub path: PathBuf,
    pub offset: usize,
    pub bytes_returned: usize,
    pub total_bytes: usize,
    pub sha256: String,
    pub truncated: bool,
    pub content: String,
}

impl ShellSpilloverStore {
    pub(crate) fn new() -> Self {
        Self::with_budget(DEFAULT_SHELL_SPILLOVER_BUDGET_BYTES)
    }

    pub(crate) fn with_budget(budget_bytes: u64) -> Self {
        let session_dir = build_session_dir();
        // Pre-create the session dir so spill() does not race on the
        // first call. Failures are non-fatal: the first spill will
        // re-attempt the create and propagate the io::Error from there.
        let _ = fs::create_dir_all(&session_dir);
        Self {
            session_dir,
            budget_bytes,
            bytes_used: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    #[cfg(test)]
    pub(crate) fn bytes_used(&self) -> u64 {
        self.bytes_used.load(Ordering::Acquire)
    }

    /// Spill the raw captured `stdout` (and `stderr` when non-empty) to
    /// a tempfile under the session directory and return the path +
    /// bytes written. Returns `None` if either the per-session budget
    /// would be exceeded or the disk write failed. Failures are
    /// non-fatal — the caller still returns the shell result without a
    /// spillover pointer.
    pub(crate) fn spill(
        &self,
        call_id: &str,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Option<ShellSpilloverInfo> {
        let bytes = encode_spill_payload(stdout, stderr);
        if bytes.is_empty() {
            return None;
        }
        let size = bytes.len() as u64;
        // Reserve budget atomically so concurrent shell calls cannot
        // race past the cap. Reservation is rolled back on write
        // failure to keep `bytes_used` honest.
        if !self.try_reserve(size) {
            return None;
        }
        if fs::create_dir_all(&self.session_dir).is_err() {
            self.release(size);
            return None;
        }
        let short_hash = &sha256_hex(&bytes)[..SPILL_SHORT_HASH_HEX];
        let sanitized = sanitize_call_id(call_id);
        let path = self
            .session_dir
            .join(format!("{sanitized}-{short_hash}.txt"));
        if fs::write(&path, &bytes).is_err() {
            self.release(size);
            return None;
        }
        Some(ShellSpilloverInfo { path, bytes: size })
    }

    /// Read a bounded byte window from a spillover path the model
    /// rediscovered from a previous result. The path is validated
    /// against the session directory so the tool cannot be redirected
    /// at arbitrary filesystem locations.
    pub(crate) fn read_range(
        &self,
        requested_path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<ShellSpilloverReadResult, String> {
        let path = self.resolve(requested_path)?;
        let bytes = fs::read(&path).map_err(|err| format!("spillover file unreadable: {err}"))?;
        let total_bytes = bytes.len();
        let offset = offset.min(total_bytes);
        let end = offset.saturating_add(limit).min(total_bytes);
        let content = String::from_utf8_lossy(&bytes[offset..end]).to_string();
        Ok(ShellSpilloverReadResult {
            path,
            offset,
            bytes_returned: end - offset,
            total_bytes,
            sha256: sha256_hex(&bytes),
            truncated: end < total_bytes,
            content,
        })
    }

    fn try_reserve(&self, size: u64) -> bool {
        loop {
            let used = self.bytes_used.load(Ordering::Acquire);
            if used.saturating_add(size) > self.budget_bytes {
                return false;
            }
            if self
                .bytes_used
                .compare_exchange(used, used + size, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }

    fn release(&self, size: u64) {
        // `fetch_sub` saturates conceptually because reservation
        // always succeeds before release: the counter can never go
        // below the reserved amount.
        self.bytes_used.fetch_sub(size, Ordering::AcqRel);
    }

    /// Resolve a model-supplied spillover path (absolute or relative)
    /// against the session directory and reject anything that resolves
    /// outside it. Symlinks are followed via canonicalize so a symlink
    /// inside the spillover dir that points elsewhere is rejected too.
    fn resolve(&self, requested: &str) -> Result<PathBuf, String> {
        if requested.is_empty() {
            return Err("spillover path must not be empty".to_string());
        }
        let candidate = Path::new(requested);
        let absolute = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.session_dir.join(candidate)
        };
        let canonical = absolute
            .canonicalize()
            .map_err(|err| format!("spillover path not found: {err}"))?;
        let session_dir_canonical = self
            .session_dir
            .canonicalize()
            .map_err(|err| format!("spillover session dir not accessible: {err}"))?;
        if !canonical.starts_with(&session_dir_canonical) {
            return Err("spillover path is outside the session directory".to_string());
        }
        Ok(canonical)
    }
}

impl Default for ShellSpilloverStore {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ShellSpilloverStore {
    fn drop(&mut self) {
        // Best-effort cleanup. A flaky temp filesystem must not panic
        // the process at shutdown, and the OS will eventually reclaim
        // anything we leave behind under its tempdir policy.
        let _ = fs::remove_dir_all(&self.session_dir);
    }
}

fn build_session_dir() -> PathBuf {
    let parent = env::temp_dir().join("squeezy-spillover");
    let pid = std::process::id();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let nonce = SESSION_NONCE.fetch_add(1, Ordering::Relaxed);
    parent.join(format!("{pid}-{now}-{nonce}"))
}

fn encode_spill_payload(stdout: &[u8], stderr: &[u8]) -> Vec<u8> {
    if stderr.is_empty() {
        return stdout.to_vec();
    }
    let mut bytes = Vec::with_capacity(stdout.len() + STDERR_SEPARATOR.len() + stderr.len());
    bytes.extend_from_slice(stdout);
    bytes.extend_from_slice(STDERR_SEPARATOR.as_bytes());
    bytes.extend_from_slice(stderr);
    bytes
}

fn sanitize_call_id(call_id: &str) -> String {
    let mut out = String::with_capacity(call_id.len());
    for ch in call_id.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "call".to_string()
    } else {
        out
    }
}

#[cfg(test)]
#[path = "shell_spillover_tests.rs"]
mod tests;
