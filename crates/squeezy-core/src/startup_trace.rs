//! Opt-in, near-zero-cost startup timing trace.
//!
//! When `SQUEEZY_STARTUP_TRACE_FILE` points at a writable path, [`init`]
//! captures a monotonic origin and each [`mark`] appends
//! `"<elapsed_micros> <label>"` to that file. With the env var unset every
//! `mark` is a single atomic load plus a branch, so the calls can stay on the
//! hot path permanently as a regression guard for time-to-interactive.
//!
//! [`elapsed_ms_for`] reads in-memory milestone durations for telemetry
//! without requiring the trace file to be set.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static ORIGIN: OnceLock<Instant> = OnceLock::new();
static TRACE_FILE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// In-memory milestone store: `(label, elapsed_ms)` pairs ordered by mark time.
static MILESTONES: Mutex<Vec<(&'static str, u64)>> = Mutex::new(Vec::new());

/// Return the elapsed-milliseconds recorded for `label`, or `None` if that
/// milestone was never marked. Used by the startup-ready telemetry path to
/// attach per-phase timings without requiring `SQUEEZY_STARTUP_TRACE_FILE`.
pub fn elapsed_ms_for(label: &'static str) -> Option<u64> {
    MILESTONES
        .lock()
        .ok()
        .and_then(|guard| guard.iter().find(|(l, _)| *l == label).map(|(_, ms)| *ms))
}

/// Record the process origin and resolve the trace destination once, at the
/// very top of `main`. Cheap and idempotent.
pub fn init() {
    let _ = ORIGIN.set(Instant::now());
    let _ = TRACE_FILE.set(std::env::var_os("SQUEEZY_STARTUP_TRACE_FILE").map(PathBuf::from));
}

/// Append one milestone to the trace file and record it in-memory.
/// The in-memory store is always updated (for telemetry) even when
/// `SQUEEZY_STARTUP_TRACE_FILE` is not set. File writes are no-ops
/// when the env var is absent.
pub fn mark(label: &'static str) {
    let Some(origin) = ORIGIN.get() else {
        return;
    };
    let elapsed = origin.elapsed();
    // Always record in-memory for telemetry.
    if let Ok(mut guard) = MILESTONES.lock() {
        guard.push((label, elapsed.as_millis() as u64));
    }
    // File write only when the trace path is configured.
    let Some(Some(path)) = TRACE_FILE.get() else {
        return;
    };
    let micros = elapsed.as_micros();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{micros} {label}");
    }
}
