//! Async wrapper around a spawned sandbox child's raw OS handles.
//!
//! Owns the process handle (and stdin write handle, if any), closing them on
//! drop, and exposes the stdout/stderr pipe read-ends as async files so the
//! caller's capture pipeline can read them like any other `tokio` stream. This
//! lives in `squeezy-win-sandbox` (rather than the consuming crate) so the
//! Win32 wait/kill/handle code is type-checked for the windows target without
//! pulling the consumer's heavier dependency tree.

use std::os::windows::io::FromRawHandle;
use std::os::windows::process::ExitStatusExt;
use std::process::ExitStatus;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::TerminateJobObject;
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, INFINITE, TerminateProcess, WaitForSingleObject,
};

use crate::{RawHandle, WinSandboxChildHandles};

/// A spawned sandbox child (restricted-token or elevated tier).
pub struct WinSandboxChild {
    pid: u32,
    process: isize,
    /// Kill-on-close Job Object the child + descendants are bound to (0 if none).
    job: isize,
    stdout: Option<tokio::fs::File>,
    stderr: Option<tokio::fs::File>,
    stdin: Option<isize>,
    waited: bool,
}

impl WinSandboxChild {
    pub(crate) fn from_handles(handles: WinSandboxChildHandles) -> Self {
        Self {
            pid: handles.pid,
            process: handles.process.0,
            job: handles.job.0,
            stdout: wrap_pipe(handles.stdout_read),
            stderr: wrap_pipe(handles.stderr_read),
            stdin: handles.stdin_write.map(|h| h.0),
            waited: false,
        }
    }

    /// The child process id (for Job Object assignment by the caller).
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Take the stdout pipe read-end as an async file (once).
    pub fn take_stdout(&mut self) -> Option<tokio::fs::File> {
        self.stdout.take()
    }

    /// Take the stderr pipe read-end as an async file (once).
    pub fn take_stderr(&mut self) -> Option<tokio::fs::File> {
        self.stderr.take()
    }

    /// Wait for the process to exit (off-thread, so the async runtime is not
    /// blocked) and return its exit status.
    pub async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        if self.waited {
            return Ok(ExitStatus::from_raw(0));
        }
        let process = self.process;
        let code = tokio::task::spawn_blocking(move || unsafe {
            let handle = process as HANDLE;
            WaitForSingleObject(handle, INFINITE);
            let mut code: u32 = 0;
            GetExitCodeProcess(handle, &mut code);
            code
        })
        .await
        .map_err(std::io::Error::other)?;
        self.waited = true;
        Ok(ExitStatus::from_raw(code))
    }

    /// Force-terminate the whole process tree (job), or the root process if no
    /// job is bound.
    pub fn kill(&self) {
        // SAFETY: `job`/`process` are live handles owned by this struct.
        unsafe {
            if self.job != 0 {
                TerminateJobObject(self.job as HANDLE, 1);
            } else {
                TerminateProcess(self.process as HANDLE, 1);
            }
        }
    }
}

impl Drop for WinSandboxChild {
    fn drop(&mut self) {
        // SAFETY: handles owned by this struct; closed exactly once on drop.
        // The stdout/stderr `tokio::fs::File`s close their own handles. Closing
        // the job handle last triggers JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        // reaping any descendants that outlived the root.
        unsafe {
            if self.process != 0 {
                CloseHandle(self.process as HANDLE);
            }
            if let Some(stdin) = self.stdin
                && stdin != 0
            {
                CloseHandle(stdin as HANDLE);
            }
            if self.job != 0 {
                CloseHandle(self.job as HANDLE);
            }
        }
    }
}

fn wrap_pipe(raw: RawHandle) -> Option<tokio::fs::File> {
    if raw.0 == 0 {
        None
    } else {
        // SAFETY: `raw` is a freshly-created, owned pipe read handle returned by
        // the spawn path; we take sole ownership of it here.
        let std_file =
            unsafe { std::fs::File::from_raw_handle(raw.0 as std::os::windows::io::RawHandle) };
        Some(tokio::fs::File::from_std(std_file))
    }
}
