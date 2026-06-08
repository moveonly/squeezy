use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::execute;
use crossterm::style::Print;

use super::*;

/// `cargo test` runs tests on a thread pool, but the process
/// environment is shared. Any test that reads or writes
/// [`WRITE_LOG_ENV`] takes this lock first so observers see a stable
/// value for the duration of the test.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    key: &'static str,
    prior: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &OsString) -> Self {
        let prior = env::var_os(key);
        // SAFETY: tests serialize on `ENV_LOCK` before mutating env vars.
        unsafe { env::set_var(key, value) };
        Self { key, prior }
    }

    fn unset(key: &'static str) -> Self {
        let prior = env::var_os(key);
        // SAFETY: tests serialize on `ENV_LOCK` before mutating env vars.
        unsafe { env::remove_var(key) };
        Self { key, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.prior.take() {
            // SAFETY: tests serialize on `ENV_LOCK` before mutating env vars.
            Some(v) => unsafe { env::set_var(self.key, v) },
            None => unsafe { env::remove_var(self.key) },
        }
    }
}

fn unique_tap_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    env::temp_dir().join(format!(
        "squeezy-tui-write-log-{label}-{}-{nanos}.log",
        std::process::id()
    ))
}

#[test]
fn from_env_returns_plain_when_env_unset() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _guard = EnvGuard::unset(WRITE_LOG_ENV);

    let writer = TerminalWriter::from_env(io::stdout());
    assert!(matches!(writer, TerminalWriter::Plain(_)));
}

#[test]
fn from_env_returns_plain_when_env_empty() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _guard = EnvGuard::set(WRITE_LOG_ENV, &OsString::from(""));

    let writer = TerminalWriter::from_env(io::stdout());
    assert!(matches!(writer, TerminalWriter::Plain(_)));
}

#[test]
fn from_env_attaches_tap_when_env_points_at_writable_path() {
    let path = unique_tap_path("from-env");
    let _ = fs::remove_file(&path);

    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _guard = EnvGuard::set(WRITE_LOG_ENV, &path.clone().into_os_string());

    let writer = TerminalWriter::from_env(io::stdout());
    assert!(matches!(writer, TerminalWriter::Tee { .. }));
    drop(writer);

    let _ = fs::remove_file(&path);
}

#[test]
fn from_env_silently_falls_back_to_plain_when_tap_path_is_unwritable() {
    // Aim the tap at a path whose parent directory does not exist.
    // The wrapper must downgrade to `Plain` rather than refuse to
    // construct, otherwise a debug-tap misconfiguration would prevent
    // the TUI from starting at all.
    let path = env::temp_dir()
        .join("squeezy-tui-write-log-nonexistent-parent")
        .join("never-created")
        .join("tap.log");
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _guard = EnvGuard::set(WRITE_LOG_ENV, &path.clone().into_os_string());

    let writer = TerminalWriter::from_env(io::stdout());
    assert!(matches!(writer, TerminalWriter::Plain(_)));
}

#[test]
fn tee_writer_records_ansi_sequences_emitted_through_backend() {
    let path = unique_tap_path("render");
    let _ = fs::remove_file(&path);

    // Use `from_optional_path` so this test does not race with other
    // tests over the shared env var.
    let mut writer =
        TerminalWriter::from_optional_path(io::stdout(), Some(path.clone().into_os_string()));

    // Emit a small "frame" through the same `execute!` write path the
    // real `TerminalGuard` uses: a clear-screen, a literal, and a
    // colored span with reset. These are exactly the byte shapes a
    // render bug investigation needs to inspect.
    execute!(
        writer,
        Print("\x1b[2J"),
        Print("hello"),
        Print("\x1b[31mred\x1b[0m"),
    )
    .expect("execute should not fail writing to a memory-backed tap");
    writer.flush().expect("flush should not fail");
    drop(writer);

    let recorded = fs::read(&path).expect("tap file should exist after writer is dropped");
    let text = String::from_utf8_lossy(&recorded);

    assert!(
        text.contains("\x1b[2J"),
        "expected clear-screen sequence in tap, got {text:?}"
    );
    assert!(
        text.contains("hello"),
        "expected literal payload in tap, got {text:?}"
    );
    assert!(
        text.contains("\x1b[31m"),
        "expected red SGR sequence in tap, got {text:?}"
    );
    assert!(
        text.contains("\x1b[0m"),
        "expected SGR reset in tap, got {text:?}"
    );

    let _ = fs::remove_file(&path);
}

#[test]
fn tee_writer_appends_across_multiple_writers_using_same_path() {
    // Append semantics matter: the debug tap is meant to accumulate
    // frames across the lifetime of a debugging session, including
    // re-launches of the TUI process. We model that here by opening
    // two writers against the same path back-to-back.
    let path = unique_tap_path("append");
    let _ = fs::remove_file(&path);
    let path_os = path.clone().into_os_string();

    let mut first = TerminalWriter::from_optional_path(io::stdout(), Some(path_os.clone()));
    first.write_all(b"first-").unwrap();
    first.flush().unwrap();
    drop(first);

    let mut second = TerminalWriter::from_optional_path(io::stdout(), Some(path_os));
    second.write_all(b"second").unwrap();
    second.flush().unwrap();
    drop(second);

    let recorded = fs::read_to_string(&path).expect("tap file should exist");
    assert_eq!(recorded, "first-second");

    let _ = fs::remove_file(&path);
}

#[test]
fn capture_writer_records_every_byte_across_writes_and_flush() {
    // The Capture variant is the in-memory counterpart to the file tap:
    // bytes handed to the writer must land in the shared sink verbatim,
    // including across multiple writes, an `execute!`-driven frame, and
    // an explicit flush. No real terminal or temp file is involved, so
    // this test is deterministic and races nothing.
    let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let mut writer = TerminalWriter::capture(Arc::clone(&sink));
    assert!(matches!(writer, TerminalWriter::Capture { .. }));

    // Plain multi-write path: each call appends to the same sink.
    writer.write_all(b"alpha-").unwrap();
    writer.write_all(b"beta-").unwrap();

    // Drive a frame through the same `execute!` write path the real
    // `TerminalGuard` uses, so the capture is exercised against the
    // exact byte shapes the TUI emits.
    execute!(
        writer,
        Print("\x1b[2J"),
        Print("gamma"),
        Print("\x1b[31mred\x1b[0m"),
    )
    .expect("execute should not fail writing to a memory-backed sink");

    writer.flush().expect("flush should not fail");

    // A single `write` must report the entire buffer as consumed: an
    // in-memory sink never short-writes.
    let n = writer.write(b"!").expect("write should not fail");
    assert_eq!(n, 1, "capture write must consume the whole buffer");

    drop(writer);

    let captured = sink.lock().unwrap();
    let expected = b"alpha-beta-\x1b[2Jgamma\x1b[31mred\x1b[0m!";
    assert_eq!(
        captured.as_slice(),
        expected,
        "captured bytes must equal exactly what was written, got {:?}",
        String::from_utf8_lossy(&captured)
    );
}
