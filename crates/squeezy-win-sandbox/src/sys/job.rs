//! Kill-on-close Job Object for sandboxed-child process-tree termination.
//!
//! Both spawn paths create the child `CREATE_SUSPENDED`, assign it to one of
//! these jobs *before* resuming, then hand the job handle to the
//! [`crate::WinSandboxChild`]. That guarantees every descendant the command
//! spawns (cmd, PowerShell, build tools, …) is bound into the job, so a
//! timeout/cancel `TerminateJobObject` — or simply dropping the handle
//! (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) — tears down the whole tree. This is
//! the Windows analog of killing a Unix process group, and applies to BOTH the
//! restricted-token and elevated tiers (the elevated child runs as another
//! user, but we hold its handle as its creator, so the assignment still works).

use std::mem;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

/// Create a kill-on-close job object and assign `process` to it.
///
/// Best-effort: returns `None` (after a warning) on any failure so the caller
/// can still run the command — just without tree-kill — rather than failing the
/// spawn outright. The caller owns the returned handle; closing it terminates
/// every process still in the job.
pub(crate) fn create_and_assign(process: HANDLE) -> Option<HANDLE> {
    // SAFETY: standard Win32 job-object creation/configuration/assignment; all
    // handles are checked and the job is closed on any partial failure.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            tracing::warn!("CreateJobObjectW failed; sandboxed child runs without tree-kill");
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            tracing::warn!(
                "SetInformationJobObject failed; sandboxed child runs without tree-kill"
            );
            CloseHandle(job);
            return None;
        }
        if AssignProcessToJobObject(job, process) == 0 {
            tracing::warn!(
                "AssignProcessToJobObject failed; sandboxed child runs without tree-kill"
            );
            CloseHandle(job);
            return None;
        }
        Some(job)
    }
}
