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

/// Environment variable whose value is a path to the debug tap log
/// file. Empty or unset means "do not tap". The file is opened in
/// append mode so successive launches in the same shell accumulate
/// frames in order; rotation/truncation is the operator's job.
pub(crate) const WRITE_LOG_ENV: &str = "SQUEEZY_TUI_WRITE_LOG";

/// Sink used as the crossterm backend writer for the TUI. Owns the
/// process stdout handle and optionally tees every byte to the debug
/// log file selected by [`WRITE_LOG_ENV`].
pub(crate) enum TerminalWriter {
    /// No log tap configured.
    Plain(io::Stdout),
    /// Tap active. Every successful stdout write is mirrored into
    /// `tap`; tap I/O errors are deliberately swallowed.
    Tee {
        stdout: io::Stdout,
        tap: BufWriter<File>,
    },
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
        match path {
            Some(p) if !p.is_empty() => match Self::open_tap(&p) {
                Ok(tap) => Self::Tee { stdout, tap },
                Err(_) => Self::Plain(stdout),
            },
            _ => Self::Plain(stdout),
        }
    }

    fn open_tap(path: &OsStr) -> io::Result<BufWriter<File>> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(BufWriter::new(file))
    }
}

impl Write for TerminalWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            TerminalWriter::Plain(stdout) => stdout.write(buf),
            TerminalWriter::Tee { stdout, tap } => {
                let written = stdout.write(buf)?;
                let _ = tap.write_all(&buf[..written]);
                Ok(written)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            TerminalWriter::Plain(stdout) => stdout.flush(),
            TerminalWriter::Tee { stdout, tap } => {
                let result = stdout.flush();
                let _ = tap.flush();
                result
            }
        }
    }
}

#[cfg(test)]
#[path = "terminal_writer_tests.rs"]
mod tests;
