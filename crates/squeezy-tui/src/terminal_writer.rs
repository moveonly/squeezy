//! Optional debug tap that mirrors every byte written through the
//! crossterm backend into a log file. Activated by setting
//! [`WRITE_LOG_ENV`] to a writable path; when unset, the wrapper is a
//! thin pass-through over [`io::Stdout`] with no extra I/O.
//!
//! The tap exists so render-bug investigations can replay the exact
//! ANSI sequence the TUI emitted for a given frame without re-running
//! the agent under a terminal recorder. It is intentionally fire-and-
//! forget: tap errors never propagate, because a debug log that
//! disrupts the visible render defeats the whole point of having one.

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use crate::metrics::ByteCounter;

/// Environment variable whose value is a path to the debug tap log
/// file. Empty or unset means "do not tap". The file is opened in
/// append mode so successive launches in the same shell accumulate
/// frames in order; rotation/truncation is the operator's job.
pub(crate) const WRITE_LOG_ENV: &str = "SQUEEZY_TUI_WRITE_LOG";

/// The byte-sink behind a [`TerminalWriter`]: the real stdout, an optional
/// file tap mirror, or an in-memory capture buffer. Split out from the wrapper
/// so the per-frame byte counter ([`TerminalWriter::counter`]) lives on the
/// wrapper and is shared by every variant without duplicating a field.
pub(crate) enum WriterKind {
    /// No log tap configured.
    Plain(io::Stdout),
    /// Tap active. Every successful stdout write is mirrored into
    /// `tap`; tap I/O errors are deliberately swallowed.
    Tee {
        stdout: io::Stdout,
        tap: BufWriter<File>,
    },
    /// Capture mode. Every byte handed to [`Write::write`] is appended
    /// to the shared `sink` buffer; the same bytes are reported as
    /// written so callers see lossless, in-memory output with no real
    /// terminal I/O. Used by tests and headless renderers that need to
    /// assert on the exact ANSI stream the TUI would emit.
    ///
    /// Constructed only via [`TerminalWriter::capture`] (tests / the
    /// `for_capture_test` guard); the targeted `allow(dead_code)` keeps the
    /// non-test build warning-clean without masking dead code in the wired
    /// `Plain`/`Tee` variants.
    #[allow(dead_code)]
    Capture { sink: Arc<Mutex<Vec<u8>>> },
}

/// Sink used as the crossterm backend writer for the TUI. Owns the
/// process stdout handle (or a tap/capture sink) and optionally counts every
/// byte emitted into a shared [`ByteCounter`] so the render-budget
/// instrumentation can report bytes-per-frame (see [`crate::metrics`]).
pub(crate) struct TerminalWriter {
    kind: WriterKind,
    /// Per-frame byte counter, bumped by every successful `write`. Shared with
    /// the `TerminalGuard`, which resets it at frame begin and reads it at
    /// frame end. `None` when no instrumentation handle is installed (the
    /// counting is then skipped entirely).
    counter: Option<ByteCounter>,
}

impl TerminalWriter {
    /// Build a writer that wraps `stdout`, consulting [`WRITE_LOG_ENV`]
    /// to decide whether to attach a debug tap. A failure to open the
    /// tap file silently degrades to the plain variant so the TUI can
    /// still start.
    pub(crate) fn from_env(stdout: io::Stdout) -> Self {
        Self::from_optional_path(stdout, std::env::var_os(WRITE_LOG_ENV))
    }

    /// Build a writer that taps to `path` when `Some` and non-empty.
    /// Exposed so tests can exercise the tap without mutating process
    /// environment, which is racy across `cargo test`'s thread pool.
    pub(crate) fn from_optional_path(stdout: io::Stdout, path: Option<OsString>) -> Self {
        let kind = match path {
            Some(p) if !p.is_empty() => match Self::open_tap(&p) {
                Ok(tap) => WriterKind::Tee { stdout, tap },
                Err(_) => WriterKind::Plain(stdout),
            },
            _ => WriterKind::Plain(stdout),
        };
        Self {
            kind,
            counter: None,
        }
    }

    /// Build a writer that captures every emitted byte into `sink`
    /// instead of touching the terminal. This is the in-memory
    /// counterpart to the file-backed [`WriterKind::Tee`] tap: it lets tests
    /// and headless renderers observe the exact byte stream without a
    /// real stdout or a temp file. The caller retains a clone of the
    /// `Arc` to read the accumulated bytes after writing.
    #[allow(dead_code)] // Test/headless-only; see the `Capture` variant.
    pub(crate) fn capture(sink: Arc<Mutex<Vec<u8>>>) -> Self {
        Self {
            kind: WriterKind::Capture { sink },
            counter: None,
        }
    }

    /// Install the shared per-frame [`ByteCounter`] so every subsequent write
    /// increments it. Called once by the `TerminalGuard` after construction;
    /// the guard keeps the other end of the `Arc` to sample bytes-per-frame.
    pub(crate) fn set_byte_counter(&mut self, counter: ByteCounter) {
        self.counter = Some(counter);
    }

    fn open_tap(path: &OsStr) -> io::Result<BufWriter<File>> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(BufWriter::new(file))
    }
}

impl Write for TerminalWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = match &mut self.kind {
            WriterKind::Plain(stdout) => stdout.write(buf)?,
            WriterKind::Tee { stdout, tap } => {
                let written = stdout.write(buf)?;
                let _ = tap.write_all(&buf[..written]);
                written
            }
            WriterKind::Capture { sink } => {
                // Accept the whole buffer: an in-memory sink never
                // short-writes. A poisoned lock is treated as a benign
                // no-op so capture failures can never disrupt a render,
                // mirroring the fire-and-forget posture of the tap.
                if let Ok(mut buffer) = sink.lock() {
                    buffer.extend_from_slice(buf);
                }
                buf.len()
            }
        };
        // Count exactly the bytes the sink accepted (so a short stdout write is
        // reflected). Relaxed: this is a per-frame statistic, never a
        // synchronization point.
        if let Some(counter) = &self.counter {
            counter.fetch_add(written as u64, Ordering::Relaxed);
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.kind {
            WriterKind::Plain(stdout) => stdout.flush(),
            WriterKind::Tee { stdout, tap } => {
                let result = stdout.flush();
                let _ = tap.flush();
                result
            }
            // The sink is written eagerly, so there is nothing to
            // flush; bytes are already visible to the holder of `sink`.
            WriterKind::Capture { .. } => Ok(()),
        }
    }
}

#[cfg(test)]
#[path = "terminal_writer_tests.rs"]
mod tests;
