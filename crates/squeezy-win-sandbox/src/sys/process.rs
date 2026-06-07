//! Spawn a child process under a restricted token with anonymous-pipe stdio.

use std::mem::size_of;
use std::path::Path;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, CreateProcessAsUserW,
};

use super::proc_thread_attr::ProcThreadAttrList;
use super::winutil::{self, ScopedHandle, to_wide_path};
use crate::{RawHandle, WinSandboxChildHandles};

/// Spawn `argv` (pre-built as a wide command line) under `token`.
///
/// Returns raw handles adopted by the caller's async capture pipeline.
pub(crate) fn spawn_with_token(
    token: HANDLE,
    command_line: &mut Vec<u16>,
    cwd: &Path,
    env_block: &[u16],
    stdin_open: bool,
) -> crate::Result<WinSandboxChildHandles> {
    // ── Create stdio pipes ───────────────────────────────────────────────────
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1, // TRUE
    };

    // stdout pipe
    let mut stdout_read: HANDLE = std::ptr::null_mut();
    let mut stdout_write: HANDLE = std::ptr::null_mut();
    if unsafe { CreatePipe(&mut stdout_read, &mut stdout_write, &sa, 0) } == 0 {
        return Err(winutil::err("CreatePipe(stdout)"));
    }
    let _stdout_read = ScopedHandle::new(stdout_read);
    let _stdout_write = ScopedHandle::new(stdout_write);

    // stderr pipe
    let mut stderr_read: HANDLE = std::ptr::null_mut();
    let mut stderr_write: HANDLE = std::ptr::null_mut();
    if unsafe { CreatePipe(&mut stderr_read, &mut stderr_write, &sa, 0) } == 0 {
        return Err(winutil::err("CreatePipe(stderr)"));
    }
    let _stderr_read = ScopedHandle::new(stderr_read);
    let _stderr_write = ScopedHandle::new(stderr_write);

    // stdin pipe
    let mut stdin_read: HANDLE = std::ptr::null_mut();
    let mut stdin_write: HANDLE = std::ptr::null_mut();
    // stdin pipe does NOT need bInheritHandle on the write end
    sa.bInheritHandle = 0;
    if unsafe { CreatePipe(&mut stdin_read, &mut stdin_write, &sa, 0) } == 0 {
        return Err(winutil::err("CreatePipe(stdin)"));
    }
    // Re-enable inheritance on the read end (child side).
    if unsafe { SetHandleInformation(stdin_read, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
        unsafe {
            CloseHandle(stdin_read);
            CloseHandle(stdin_write);
        }
        return Err(winutil::err("SetHandleInformation(stdin_read inherit)"));
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
            return Err(winutil::err(&format!(
                "SetHandleInformation({label} non-inherit)"
            )));
        }
    }

    // ── Build STARTUPINFOEXW + attribute list ────────────────────────────────
    // Scope handle inheritance to exactly the three child-side stdio ends.
    // `attrs` must remain alive past the CreateProcessAsUserW call because Win32
    // keeps a pointer into it until the call returns.
    let mut attrs = ProcThreadAttrList::new(1)?;
    attrs.set_handle_list(&[stdin_read, stdout_write, stderr_write])?;

    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = stdin_read;
    si.StartupInfo.hStdOutput = stdout_write;
    si.StartupInfo.hStdError = stderr_write;
    si.lpAttributeList = attrs.as_mut_ptr();

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let cwd_wide = to_wide_path(cwd);

    // env_block is `&[u16]`; CreateProcessAsUserW wants `*const c_void`.
    let env_ptr = env_block.as_ptr() as *const core::ffi::c_void;

    // EXTENDED_STARTUPINFO_PRESENT tells the kernel that lpStartupInfo points to
    // a STARTUPINFOEXW and to honour the attribute list.  bInheritHandles = TRUE
    // is still required (the attribute list scopes which handles are inherited,
    // but handle inheritance must still be globally enabled).
    let ok = unsafe {
        CreateProcessAsUserW(
            token,
            std::ptr::null(),          // lpApplicationName — use command line
            command_line.as_mut_ptr(),
            std::ptr::null(),          // lpProcessAttributes
            std::ptr::null(),          // lpThreadAttributes
            1,                         // bInheritHandles = TRUE
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
            env_ptr as *mut _,
            cwd_wide.as_ptr(),
            // Cast STARTUPINFOEXW* → STARTUPINFOW*: the API accepts the extended
            // struct when EXTENDED_STARTUPINFO_PRESENT is in the creation flags.
            std::ptr::addr_of!(si.StartupInfo),
            &mut pi,
        )
    };

    // `attrs` stays alive until here; Win32 is done with the pointer.
    drop(attrs);

    if ok == 0 {
        let code = unsafe { GetLastError() };
        return Err(crate::WinSandboxError::win32(format!(
            "CreateProcessAsUserW failed: code={code}"
        )));
    }

    // ── Close child-side handles in the parent and hThread ───────────────────
    // These are now owned by _stdout_write / _stderr_write / _stdin_read above
    // so they'll close when those ScopedHandles drop — but we need to drop them
    // NOW before returning, which they will at the end of this scope.
    // Also close hThread immediately.
    unsafe {
        CloseHandle(pi.hThread);
    }

    // ── Consume ScopedHandles to get raw values ──────────────────────────────
    // Parent-side read ends: leak so caller owns them.
    let raw_stdout_read = _stdout_read.into_raw();
    let raw_stderr_read = _stderr_read.into_raw();
    let raw_stdin_write = _stdin_write.into_raw();

    // Child-side ends drop here (stdout_write, stderr_write, stdin_read).
    // _stdout_write and _stderr_write and _stdin_read all drop at end of scope.

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
    })
}
