//! Opt-in, near-zero-cost startup timing trace.
//!
//! When `SQUEEZY_STARTUP_TRACE_FILE` points at a writable path, [`init`]
//! captures a monotonic origin and each [`mark`] appends
//! `"<elapsed_micros> <label>"` to that file. With the env var unset every
//! `mark` is a single atomic load plus a branch, so the calls can stay on the
//! hot path permanently as a regression guard for time-to-interactive.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

static ORIGIN: OnceLock<Instant> = OnceLock::new();
static TRACE_FILE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Record the process origin and resolve the trace destination once, at the
/// very top of `main`. Cheap and idempotent.
pub fn init() {
    let _ = ORIGIN.set(Instant::now());
    let _ = TRACE_FILE.set(std::env::var_os("SQUEEZY_STARTUP_TRACE_FILE").map(PathBuf::from));
}

/// Append one milestone to the trace file. No-op unless [`init`] ran with the
/// env var set.
pub fn mark(label: &str) {
    let Some(origin) = ORIGIN.get() else {
        return;
    };
    let Some(Some(path)) = TRACE_FILE.get() else {
        return;
    };
    let micros = origin.elapsed().as_micros();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{micros} {label}");
    }
}
