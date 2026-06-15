use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex, mpsc},
    thread,
};

use fs2::FileExt as _;
use squeezy_core::{CacheDurability, Result, SqueezyError};

use crate::sync_parent_dir;

use super::{SessionMetadata, SessionStatus, SessionStore, read_session_metadata};

#[derive(Debug)]
pub(super) struct SessionLogAppend {
    pub(super) payload: Vec<u8>,
}

enum SessionLogCmd {
    Append(SessionLogAppend),
    /// Route `replay.jsonl` writes through the same queued writer as
    /// `events.jsonl` so concurrent replay appends serialise their I/O
    /// and avoid per-write open/close churn on Windows.
    AppendReplay(SessionLogAppend),
    Flush {
        ack: mpsc::Sender<Result<()>>,
    },
    Shutdown {
        ack: mpsc::Sender<Result<()>>,
    },
}

#[derive(Debug)]
pub(super) struct SessionLogWriter {
    tx: mpsc::Sender<SessionLogCmd>,
    worker: StdMutex<Option<thread::JoinHandle<()>>>,
    terminal_failure: StdMutex<Option<String>>,
}

impl SessionLogWriter {
    pub(super) fn spawn(store: SessionStore, dir: PathBuf) -> Arc<Self> {
        let (tx, rx) = mpsc::channel();
        let terminal_failure = StdMutex::new(None);
        let writer = Arc::new(Self {
            tx,
            worker: StdMutex::new(None),
            terminal_failure,
        });
        let failure = Arc::downgrade(&writer);
        let worker = thread::spawn(move || {
            run_session_log_writer(store, dir, rx, failure);
        });
        *writer.worker.lock().expect("session log writer worker") = Some(worker);
        writer
    }

    pub(super) fn append(&self, append: SessionLogAppend) -> Result<()> {
        self.check_failure()?;
        self.tx
            .send(SessionLogCmd::Append(append))
            .map_err(|_| SqueezyError::Agent("session log writer stopped".to_string()))?;
        self.check_failure()
    }

    pub(super) fn append_replay(&self, append: SessionLogAppend) -> Result<()> {
        self.check_failure()?;
        self.tx
            .send(SessionLogCmd::AppendReplay(append))
            .map_err(|_| SqueezyError::Agent("session log writer stopped".to_string()))?;
        self.check_failure()
    }

    pub(super) fn flush(&self) -> Result<()> {
        self.check_failure()?;
        let (ack, rx) = mpsc::channel();
        self.tx
            .send(SessionLogCmd::Flush { ack })
            .map_err(|_| SqueezyError::Agent("session log writer stopped".to_string()))?;
        rx.recv()
            .map_err(|_| SqueezyError::Agent("session log writer stopped".to_string()))?
    }

    fn record_failure(&self, error: impl ToString) {
        let mut failure = self
            .terminal_failure
            .lock()
            .expect("session log writer failure");
        if failure.is_none() {
            *failure = Some(error.to_string());
        }
    }

    fn check_failure(&self) -> Result<()> {
        if let Some(error) = self
            .terminal_failure
            .lock()
            .expect("session log writer failure")
            .clone()
        {
            return Err(SqueezyError::Io(std::io::Error::other(error)));
        }
        Ok(())
    }
}

impl Drop for SessionLogWriter {
    fn drop(&mut self) {
        let (ack, rx) = mpsc::channel();
        let _ = self.tx.send(SessionLogCmd::Shutdown { ack });
        let _ = rx.recv();
        if let Some(worker) = self
            .worker
            .lock()
            .expect("session log writer worker")
            .take()
        {
            let _ = worker.join();
        }
    }
}

fn run_session_log_writer(
    store: SessionStore,
    dir: PathBuf,
    rx: mpsc::Receiver<SessionLogCmd>,
    writer: std::sync::Weak<SessionLogWriter>,
) {
    let path = dir.join("events.jsonl");
    let mut current_size = fs::metadata(&path).map_or(0, |metadata| metadata.len() as usize);
    let replay_path = dir.join("replay.jsonl");
    let mut replay_current_size =
        fs::metadata(&replay_path).map_or(0, |metadata| metadata.len() as usize);
    let mut terminal_failure: Option<String> = None;
    let mut truncated = false;
    let mut replay_truncated = false;
    for command in rx {
        match command {
            SessionLogCmd::Append(append) => {
                if terminal_failure.is_some() {
                    continue;
                }
                if let Err(error) = write_session_log_append(
                    &store,
                    &dir,
                    &path,
                    &mut current_size,
                    &mut truncated,
                    append,
                ) {
                    let message = error.to_string();
                    if let Some(writer) = writer.upgrade() {
                        writer.record_failure(&message);
                    }
                    terminal_failure = Some(message);
                }
            }
            SessionLogCmd::AppendReplay(append) => {
                if terminal_failure.is_some() {
                    continue;
                }
                if let Err(error) = write_replay_log_append(
                    &store,
                    &dir,
                    &replay_path,
                    &mut replay_current_size,
                    &mut replay_truncated,
                    append,
                ) {
                    let message = error.to_string();
                    if let Some(writer) = writer.upgrade() {
                        writer.record_failure(&message);
                    }
                    terminal_failure = Some(message);
                }
            }
            SessionLogCmd::Flush { ack } => {
                if terminal_failure.is_none()
                    && matches!(
                        store.durability,
                        CacheDurability::Turn | CacheDurability::Strict
                    )
                    && let Err(error) = sync_file_if_exists(&path)
                        .and_then(|()| sync_parent_dir(&path))
                        .and_then(|()| sync_file_if_exists(&replay_path))
                        .and_then(|()| sync_parent_dir(&replay_path))
                {
                    let message = error.to_string();
                    if let Some(writer) = writer.upgrade() {
                        writer.record_failure(&message);
                    }
                    terminal_failure = Some(message);
                }
                let _ = ack.send(session_log_writer_result(terminal_failure.as_deref()));
            }
            SessionLogCmd::Shutdown { ack } => {
                if terminal_failure.is_none()
                    && matches!(
                        store.durability,
                        CacheDurability::Turn | CacheDurability::Strict
                    )
                    && let Err(error) = sync_file_if_exists(&path)
                        .and_then(|()| sync_parent_dir(&path))
                        .and_then(|()| sync_file_if_exists(&replay_path))
                        .and_then(|()| sync_parent_dir(&replay_path))
                {
                    let message = error.to_string();
                    if let Some(writer) = writer.upgrade() {
                        writer.record_failure(&message);
                    }
                    terminal_failure = Some(message);
                }
                let _ = ack.send(session_log_writer_result(terminal_failure.as_deref()));
                break;
            }
        }
    }
}

fn session_log_writer_result(failure: Option<&str>) -> Result<()> {
    if let Some(failure) = failure {
        return Err(SqueezyError::Io(std::io::Error::other(failure.to_string())));
    }
    Ok(())
}

fn write_session_log_append(
    store: &SessionStore,
    dir: &Path,
    path: &Path,
    current_size: &mut usize,
    truncated: &mut bool,
    append: SessionLogAppend,
) -> Result<()> {
    fs::create_dir_all(dir)?;
    if current_size.saturating_add(append.payload.len()) > store.max_session_bytes {
        // Record the truncation transition exactly once. `current_size` only
        // ever grows, so every later append would otherwise re-enter this
        // branch and rewrite byte-identical metadata.json for the rest of the
        // session.
        if !*truncated {
            update_metadata_file(store, dir, |metadata| {
                metadata.status = SessionStatus::Truncated;
                metadata.resume_available = false;
                metadata.resume_unavailable_reason =
                    Some("session exceeded max_session_bytes".to_string());
            })?;
            *truncated = true;
        }
        return Ok(());
    }
    append_payload_with_recovery(path, &append.payload, current_size)?;
    if store.durability == CacheDurability::Strict {
        sync_file_if_exists(path)?;
        sync_parent_dir(path)?;
    }
    Ok(())
}

/// Writer-thread handler for `replay.jsonl` appends. Mirrors
/// `write_session_log_append` but targets the replay file and uses the
/// "replay trace exceeded" reason string on truncation, matching the
/// message emitted by the old direct-write path.
fn write_replay_log_append(
    store: &SessionStore,
    dir: &Path,
    replay_path: &Path,
    replay_current_size: &mut usize,
    replay_truncated: &mut bool,
    append: SessionLogAppend,
) -> Result<()> {
    fs::create_dir_all(dir)?;
    if replay_current_size.saturating_add(append.payload.len()) > store.max_session_bytes {
        if !*replay_truncated {
            update_metadata_file(store, dir, |metadata| {
                metadata.status = SessionStatus::Truncated;
                metadata.resume_available = false;
                metadata.resume_unavailable_reason =
                    Some("replay trace exceeded max_session_bytes".to_string());
            })?;
            *replay_truncated = true;
        }
        return Ok(());
    }
    append_payload_with_recovery(replay_path, &append.payload, replay_current_size)?;
    if store.durability == CacheDurability::Strict {
        sync_file_if_exists(replay_path)?;
        sync_parent_dir(replay_path)?;
    }
    Ok(())
}

pub(super) fn append_payload_with_recovery(
    path: &Path,
    payload: &[u8],
    current_size: &mut usize,
) -> std::io::Result<()> {
    match append_payload_once(path, payload) {
        Ok(written) => {
            *current_size = current_size.saturating_add(written);
            Ok(())
        }
        Err((written, first_error)) => {
            *current_size = current_size.saturating_add(written);
            if written > 0 {
                return Err(first_error);
            }
            match append_payload_once(path, payload) {
                Ok(written) => {
                    *current_size = current_size.saturating_add(written);
                    Ok(())
                }
                Err((retry_written, retry_error)) => {
                    *current_size = current_size.saturating_add(retry_written);
                    Err(retry_error)
                }
            }
        }
    }
}

fn append_payload_once(
    path: &Path,
    payload: &[u8],
) -> std::result::Result<usize, (usize, std::io::Error)> {
    let lock = lock_append_path(path).map_err(|error| (0, error))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| (0, error))?;
    let mut written = 0;
    let result = (|| {
        while written < payload.len() {
            match file.write(&payload[written..]) {
                Ok(0) => {
                    return Err((
                        written,
                        std::io::Error::new(std::io::ErrorKind::WriteZero, "failed to write event"),
                    ));
                }
                Ok(bytes) => written += bytes,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err((written, error)),
            }
        }
        Ok(written)
    })();
    // Unlock best-effort; the OS releases the lock when `lock` drops regardless.
    let _ = lock.unlock();
    result
}

pub(super) fn sync_file_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::File::open(path) {
        Ok(file) => file.sync_all(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn update_metadata_file(
    store: &SessionStore,
    dir: &Path,
    update: impl FnOnce(&mut SessionMetadata),
) -> Result<()> {
    let path = dir.join("metadata.json");
    let mut metadata = read_session_metadata(&path)?;
    update(&mut metadata);
    store.write_metadata_file(dir, &metadata)
}

fn lock_append_path(path: &Path) -> std::io::Result<fs::File> {
    let lock_path = append_lock_path(path);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    // `truncate(false)` is the `OpenOptions` default, but clippy's
    // `suspicious_open_options` lint requires an explicit choice when
    // `create(true).write(true)` is set so a reader of the call site does
    // not have to remember which side of the default the call falls on.
    // Lock files are zero-byte sentinels; preserving any contents is fine.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)?;
    lock.lock_exclusive()?;
    Ok(lock)
}

fn append_lock_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("append");
    path.with_file_name(format!(".{name}.lock"))
}
