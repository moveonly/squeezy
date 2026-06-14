use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use squeezy_core::{Redactor, StreamRedactor};
use tokio::{io::AsyncReadExt, sync::Mutex, time};

use crate::shell_spillover::RawSidecar;

#[derive(Clone, Default)]
pub(crate) struct ShellStreamCapture {
    bytes: Arc<Mutex<Vec<u8>>>,
    len: Arc<AtomicUsize>,
    truncated: Arc<AtomicBool>,
}

impl ShellStreamCapture {
    async fn append(&self, chunk: &[u8], cap: usize) {
        if chunk.is_empty() {
            return;
        }
        if self.len.load(Ordering::Acquire) >= cap {
            self.truncated.store(true, Ordering::Relaxed);
            return;
        }
        let mut bytes = self.bytes.lock().await;
        let keep = chunk.len().min(cap.saturating_sub(bytes.len()));
        if keep > 0 {
            bytes.extend_from_slice(&chunk[..keep]);
            self.len.store(bytes.len(), Ordering::Release);
        }
        if keep < chunk.len() {
            self.truncated.store(true, Ordering::Relaxed);
        }
    }

    fn mark_truncated(&self) {
        self.truncated.store(true, Ordering::Relaxed);
    }

    async fn snapshot(&self) -> (Vec<u8>, bool) {
        (
            self.bytes.lock().await.clone(),
            self.truncated.load(Ordering::Relaxed),
        )
    }
}

pub(crate) async fn read_limited_pipe<R>(
    mut reader: Option<R>,
    cap: usize,
    capture: ShellStreamCapture,
    raw_sidecar: Option<RawSidecar>,
    redactor: Arc<Redactor>,
) -> std::result::Result<(), std::io::Error>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(mut reader) = reader.take() else {
        return Ok(());
    };
    let mut buffer = vec![0u8; 8192];
    let mut sidecar = raw_sidecar.map(|sink| RawStreamMirror::new(sink, redactor));

    loop {
        let count = match reader.read(&mut buffer).await {
            Ok(count) => count,
            Err(err) if err.raw_os_error() == Some(libc::EIO) => break,
            Err(err) => return Err(err),
        };
        if count == 0 {
            break;
        }
        if let Some(sidecar) = sidecar.as_mut() {
            sidecar.ingest(&buffer[..count], cap).await;
        }
        capture.append(&buffer[..count], cap).await;
    }

    if let Some(sidecar) = sidecar.as_mut() {
        sidecar.finish(cap).await;
    }

    Ok(())
}

struct RawStreamMirror {
    sink: RawSidecar,
    redactor: StreamRedactor,
    pending: String,
    overflowed: bool,
}

impl RawStreamMirror {
    fn new(sink: RawSidecar, redactor: Arc<Redactor>) -> Self {
        Self {
            sink,
            redactor: StreamRedactor::new(redactor),
            pending: String::new(),
            overflowed: false,
        }
    }

    async fn ingest(&mut self, chunk: &[u8], cap: usize) {
        let over = self.sink.note_raw_and_overflowed(chunk.len(), cap).await;
        let emitted = self.redactor.push(&String::from_utf8_lossy(chunk)).text;
        if self.overflowed {
            self.sink.write_chunk(&emitted).await;
            return;
        }
        self.pending.push_str(&emitted);
        if over {
            self.flush_pending().await;
        }
    }

    async fn finish(&mut self, cap: usize) {
        let tail = self.redactor.finish().text;
        let over = self.sink.note_raw_and_overflowed(0, cap).await;
        if self.overflowed || over {
            if !self.overflowed {
                self.flush_pending().await;
            }
            self.sink.write_chunk(&tail).await;
        }
    }

    async fn flush_pending(&mut self) {
        self.overflowed = true;
        let pending = std::mem::take(&mut self.pending);
        self.sink.write_chunk(&pending).await;
    }
}

pub(crate) async fn drain_or_abort(
    mut handle: tokio::task::JoinHandle<std::result::Result<(), std::io::Error>>,
    capture: ShellStreamCapture,
    timeout: Duration,
) -> std::result::Result<(Vec<u8>, bool), std::io::Error> {
    match time::timeout(timeout, &mut handle).await {
        Ok(joined) => {
            joined.map_err(|err| {
                std::io::Error::other(format!("shell output reader failed: {err}"))
            })??;
        }
        Err(_) => {
            handle.abort();
            capture.mark_truncated();
        }
    }
    Ok(capture.snapshot().await)
}

pub(crate) fn split_shell_output(
    mut stdout: Vec<u8>,
    stdout_truncated: bool,
    mut stderr: Vec<u8>,
    stderr_truncated: bool,
    output_cap: usize,
) -> (Vec<u8>, bool, Vec<u8>, bool) {
    if output_cap == 0 || stdout.len().saturating_add(stderr.len()) <= output_cap {
        return (stdout, stdout_truncated, stderr, stderr_truncated);
    }

    let stdout_floor = if output_cap >= 24 * 1024 {
        (output_cap / 3).max(8 * 1024)
    } else {
        (output_cap / 3).max(1)
    }
    .min(output_cap);
    let stdout_len = stdout.len();
    let stderr_len = stderr.len();
    let mut stdout_take = stdout_len.min(stdout_floor);
    let mut stderr_take = stderr_len.min(output_cap.saturating_sub(stdout_take));
    let mut remaining = output_cap.saturating_sub(stdout_take + stderr_take);
    let extra_stdout = remaining.min(stdout_len.saturating_sub(stdout_take));
    stdout_take += extra_stdout;
    remaining = remaining.saturating_sub(extra_stdout);
    let extra_stderr = remaining.min(stderr_len.saturating_sub(stderr_take));
    stderr_take += extra_stderr;

    let final_stdout_truncated = stdout_truncated || stdout_take < stdout_len;
    let final_stderr_truncated = stderr_truncated || stderr_take < stderr_len;
    stdout.truncate(stdout_take);
    stderr.truncate(stderr_take);
    (
        stdout,
        final_stdout_truncated,
        stderr,
        final_stderr_truncated,
    )
}
