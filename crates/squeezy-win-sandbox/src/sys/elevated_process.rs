//! Spawn a child process under a different user account via `CreateProcessWithLogonW`
//! with anonymous-pipe stdio, mirroring the pipe setup in `process.rs`.

use std::ffi::c_void;
use std::path::Path;

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessWithLogonW,
    LOGON_WITH_PROFILE, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOW,
};

use super::winutil::{ScopedHandle, err, to_wide, to_wide_path};
use crate::{RawHandle, WinSandboxChildHandles, WinSandboxError};

/// Spawn `command_line` (pre-built wide NUL-terminated string) as `username`
/// with `password`, creating anonymous pipes for stdio.
///
/// Mirrors `process::spawn_with_token` exactly for the pipe + `STARTUPINFOW` +
/// handle-cleanup logic, substituting `CreateProcessWithLogonW` for
/// `CreateProcessAsUserW`.
///
/// `CreateProcessWithLogonW` has no `bInheritHandles` parameter; instead it
/// inherits all inheritable handles in the calling process when
/// `STARTF_USESTDHANDLES` is set.  The child-side pipe ends are created
/// inheritable (`SECURITY_ATTRIBUTES { bInheritHandle: 1 }`); the parent-side
/// read/write ends are then explicitly made non-inheritable via
/// `SetHandleInformation` so only the three desired handles cross the logon
/// boundary.
pub(crate) fn spawn_with_logon(
    username: &str,
    password: &str,
    command_line: &mut Vec<u16>,
    cwd: &Path,
    env_block: &[u16],
    stdin_open: bool,
) -> crate::Result<WinSandboxChildHandles> {
    // ── Encode credentials ───────────────────────────────────────────────────
    let user_wide = to_wide(username);
    let domain_wide = to_wide(".");

    // Build a mutable Vec so we can zero it after the call.
    let mut pass_wide: Vec<u16> = to_wide(password);

    // ── Create stdio pipes ───────────────────────────────────────────────────
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1, // TRUE — child-side ends inherit across the logon
    };

    // stdout pipe (child inherits write end)
    let mut stdout_read: HANDLE = std::ptr::null_mut();
    let mut stdout_write: HANDLE = std::ptr::null_mut();
    if unsafe { CreatePipe(&mut stdout_read, &mut stdout_write, &sa, 0) } == 0 {
        return Err(err("CreatePipe(stdout)"));
    }
    let _stdout_read = ScopedHandle::new(stdout_read);
    let _stdout_write = ScopedHandle::new(stdout_write);

    // stderr pipe (child inherits write end)
    let mut stderr_read: HANDLE = std::ptr::null_mut();
    let mut stderr_write: HANDLE = std::ptr::null_mut();
    if unsafe { CreatePipe(&mut stderr_read, &mut stderr_write, &sa, 0) } == 0 {
        return Err(err("CreatePipe(stderr)"));
    }
    let _stderr_read = ScopedHandle::new(stderr_read);
    let _stderr_write = ScopedHandle::new(stderr_write);

    // stdin pipe: write end (parent side) must NOT be inheritable; read end
    // (child side) must be inheritable.
    sa.bInheritHandle = 0;
    let mut stdin_read: HANDLE = std::ptr::null_mut();
    let mut stdin_write: HANDLE = std::ptr::null_mut();
    if unsafe { CreatePipe(&mut stdin_read, &mut stdin_write, &sa, 0) } == 0 {
        return Err(err("CreatePipe(stdin)"));
    }
    // Re-enable inheritance on the read end (child side).
    if unsafe { SetHandleInformation(stdin_read, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
        unsafe {
            CloseHandle(stdin_read);
            CloseHandle(stdin_write);
        }
        return Err(err("SetHandleInformation(stdin_read inherit)"));
    }
    let _stdin_read = ScopedHandle::new(stdin_read);
    let _stdin_write = ScopedHandle::new(stdin_write);

    // ── Mark parent-side ends non-inheritable ────────────────────────────────
    for (h, label) in [
        (stdout_read, "stdout_read"),
        (stderr_read, "stderr_read"),
        (stdin_write, "stdin_write"),
    ] {
        if unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(err(&format!("SetHandleInformation({label} non-inherit)")));
        }
    }

    // ── Build STARTUPINFOW ───────────────────────────────────────────────────
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = stdin_read;
    si.hStdOutput = stdout_write;
    si.hStdError = stderr_write;

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let cwd_wide = to_wide_path(cwd);

    // `CreateProcessWithLogonW` takes `*const c_void` for `lpEnvironment`.
    let env_ptr = env_block.as_ptr() as *const c_void;

    // ── Call CreateProcessWithLogonW ─────────────────────────────────────────
    //
    // The function has no `bInheritHandles` parameter.  According to the Win32
    // documentation, when `STARTF_USESTDHANDLES` is set in `STARTUPINFO` the
    // three stdio handles are always inherited by the child (provided they are
    // marked inheritable).  All other inheritable handles in the process are
    // also inherited, which is why we explicitly clear the inherit flag on the
    // parent-side pipe ends above.
    let ok = unsafe {
        CreateProcessWithLogonW(
            user_wide.as_ptr(),
            domain_wide.as_ptr(),
            pass_wide.as_ptr(),
            LOGON_WITH_PROFILE,
            std::ptr::null(), // lpApplicationName — use command line
            command_line.as_mut_ptr(),
            // CREATE_SUSPENDED so the child is bound to its kill-on-close Job
            // Object before it can spawn descendants (no escape race) — the
            // elevated tier MUST have tree-kill just like every other backend.
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | CREATE_SUSPENDED,
            env_ptr,
            cwd_wide.as_ptr(),
            &si,
            &mut pi,
        )
    };

    // Zero the password buffer immediately after the call (best-effort
    // mitigation against plaintext passwords lingering in memory).
    for w in pass_wide.iter_mut() {
        *w = 0;
    }

    if ok == 0 {
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(WinSandboxError::win32(format!(
            "CreateProcessWithLogonW failed: code={code}"
        )));
    }

    // ── Bind to a kill-on-close Job Object, then resume ──────────────────────
    // We created the process, so we hold `pi.hProcess` with full rights and can
    // assign it to our job even though it runs as the sandbox user. Doing this
    // while suspended binds every descendant into the job. Best-effort.
    let job = super::job::create_and_assign(pi.hProcess);
    unsafe {
        ResumeThread(pi.hThread);
        CloseHandle(pi.hThread);
    }

    // ── Consume ScopedHandles — leak parent-side ends to caller ─────────────
    let raw_stdout_read = _stdout_read.into_raw();
    let raw_stderr_read = _stderr_read.into_raw();
    let raw_stdin_write = _stdin_write.into_raw();

    // Child-side ends (_stdout_write, _stderr_write, _stdin_read) drop here.

    let stdin_write_out = if stdin_open {
        Some(RawHandle(raw_stdin_write as isize))
    } else {
        unsafe {
            CloseHandle(raw_stdin_write);
        }
        None
    };

    Ok(WinSandboxChildHandles {
        pid: pi.dwProcessId,
        process: RawHandle(pi.hProcess as isize),
        stdout_read: RawHandle(raw_stdout_read as isize),
        stderr_read: RawHandle(raw_stderr_read as isize),
        stdin_write: stdin_write_out,
        job: RawHandle(job.map_or(0, |j| j as isize)),
    })
}
