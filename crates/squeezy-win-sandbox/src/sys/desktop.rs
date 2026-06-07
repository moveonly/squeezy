//! Grant a sandbox user access to the current process's window station and
//! thread desktop so that `CreateProcessWithLogonW` can initialise the process
//! environment without a permission failure.
//!
//! This is best-effort: a `tracing::warn!` is emitted on failure and `Ok(())` is
//! returned regardless, because non-interactive (console-only) commands usually
//! work without these grants; refusing to spawn over a DACL-update failure would
//! be worse than attempting the spawn with a warning.

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::Security::ACL;
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetSecurityInfo, SE_WINDOW_OBJECT, SetEntriesInAclW,
    SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, OBJECT_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    DESKTOP_CREATEMENU, DESKTOP_CREATEWINDOW, DESKTOP_DELETE, DESKTOP_ENUMERATE,
    DESKTOP_HOOKCONTROL, DESKTOP_JOURNALPLAYBACK, DESKTOP_JOURNALRECORD, DESKTOP_READ_CONTROL,
    DESKTOP_READOBJECTS, DESKTOP_SWITCHDESKTOP, DESKTOP_SYNCHRONIZE, DESKTOP_WRITE_DAC,
    DESKTOP_WRITE_OWNER, DESKTOP_WRITEOBJECTS, GetProcessWindowStation, GetThreadDesktop,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::WINSTA_ALL_ACCESS;

use super::winutil::OwnedSid;

/// Full desktop access mask — mirrors the Codex reference implementation's
/// `DESKTOP_ALL_ACCESS`.
const DESKTOP_FULL: u32 = DESKTOP_READOBJECTS
    | DESKTOP_CREATEWINDOW
    | DESKTOP_CREATEMENU
    | DESKTOP_HOOKCONTROL
    | DESKTOP_JOURNALRECORD
    | DESKTOP_JOURNALPLAYBACK
    | DESKTOP_ENUMERATE
    | DESKTOP_WRITEOBJECTS
    | DESKTOP_SWITCHDESKTOP
    | DESKTOP_DELETE
    | DESKTOP_READ_CONTROL
    | DESKTOP_WRITE_DAC
    | DESKTOP_WRITE_OWNER
    | DESKTOP_SYNCHRONIZE;

/// Add an allow ACE for `user_sid` to `obj`'s DACL, MERGING into the object's
/// existing DACL (read-modify-write).
///
/// `obj` may be either an `HWINSTA` or an `HDESK` — both are window-station-
/// class kernel objects accepted by `Get/SetSecurityInfo` as `SE_WINDOW_OBJECT`.
///
/// Critically, the existing DACL is fetched via `GetSecurityInfo` and passed as
/// the *old* ACL to `SetEntriesInAclW`, so the result is the union of the
/// current ACEs plus our grant. Building from a NULL old-ACL would produce a
/// DACL containing only our single ACE and `SetSecurityInfo` would then REPLACE
/// the object's DACL — stripping the interactive user's and SYSTEM's own access
/// to the window station/desktop. We must never do that.
fn grant_object_access(obj: HANDLE, user_sid: &OwnedSid, access: u32, label: &str) {
    let si: OBJECT_SECURITY_INFORMATION = DACL_SECURITY_INFORMATION;

    // Read the existing DACL. `old_dacl` points *into* `psd`, which we LocalFree.
    let mut old_dacl: *mut ACL = std::ptr::null_mut();
    let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let grc = unsafe {
        GetSecurityInfo(
            obj,
            SE_WINDOW_OBJECT,
            si,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut old_dacl,
            std::ptr::null_mut(),
            &mut psd,
        )
    };
    if grc != ERROR_SUCCESS {
        tracing::warn!(
            label,
            code = grc,
            "grant_user_winsta_desktop: GetSecurityInfo failed; skipping grant"
        );
        return;
    }

    // Build the EXPLICIT_ACCESS entry for our grant.
    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: access,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: 0,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            // ptstrName is `*mut u16`; the SID pointer is reinterpreted as
            // per-MSDN convention when TrusteeForm == TRUSTEE_IS_SID.
            ptstrName: user_sid.as_psid() as *mut u16,
        },
    };

    // Merge our grant into the EXISTING DACL.
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    let rc = unsafe { SetEntriesInAclW(1, &ea, old_dacl, &mut new_dacl) };
    if rc != ERROR_SUCCESS {
        tracing::warn!(
            label,
            code = rc,
            "grant_user_winsta_desktop: SetEntriesInAclW failed; continuing anyway"
        );
        if !psd.is_null() {
            unsafe { LocalFree(psd as HLOCAL) };
        }
        return;
    }

    let rc2 = unsafe {
        SetSecurityInfo(
            obj,
            SE_WINDOW_OBJECT,
            si,
            std::ptr::null_mut(), // owner — not changing
            std::ptr::null_mut(), // group — not changing
            new_dacl,
            std::ptr::null_mut(), // sacl — not changing
        )
    };

    // Free both the merged DACL and the descriptor backing the old DACL.
    if !new_dacl.is_null() {
        unsafe { LocalFree(new_dacl as HLOCAL) };
    }
    if !psd.is_null() {
        unsafe { LocalFree(psd as HLOCAL) };
    }

    if rc2 != ERROR_SUCCESS {
        tracing::warn!(
            label,
            code = rc2,
            "grant_user_winsta_desktop: SetSecurityInfo failed; continuing anyway"
        );
    }
}

/// Grant the user identified by `user_sid_str` (a string-format SID such as
/// `"S-1-5-21-…"`) access to:
/// - the current process's window station (`GetProcessWindowStation`), and
/// - the current thread's desktop  (`GetThreadDesktop`).
///
/// This is required so that a process launched as a different user via
/// `CreateProcessWithLogonW` in the same session can connect to the window
/// station/desktop during its DLL-init phase.  On a headless/service setup this
/// call may not be needed, but it is harmless.
///
/// Returns `Ok(())` unconditionally; any Win32 failure is logged via
/// `tracing::warn!` so callers can proceed to attempt the spawn.
pub(crate) fn grant_user_winsta_desktop(user_sid_str: &str) -> crate::Result<()> {
    // Convert the string SID.  This is the only step that can legitimately
    // propagate an error, since a bad SID string means we have nothing to grant.
    let owned_sid = match OwnedSid::from_str(user_sid_str) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                sid = user_sid_str,
                err = %e,
                "grant_user_winsta_desktop: ConvertStringSidToSidW failed; \
                 continuing without desktop grant"
            );
            return Ok(());
        }
    };

    // ── Window station ───────────────────────────────────────────────────────
    let hwinsta = unsafe { GetProcessWindowStation() };
    if hwinsta.is_null() {
        tracing::warn!(
            "grant_user_winsta_desktop: GetProcessWindowStation returned NULL; \
             skipping winsta grant"
        );
    } else {
        grant_object_access(
            hwinsta as HANDLE,
            &owned_sid,
            WINSTA_ALL_ACCESS as u32,
            "window-station",
        );
    }

    // ── Desktop ──────────────────────────────────────────────────────────────
    let thread_id = unsafe { GetCurrentThreadId() };
    let hdesk = unsafe { GetThreadDesktop(thread_id) };
    if hdesk.is_null() {
        tracing::warn!(
            "grant_user_winsta_desktop: GetThreadDesktop returned NULL; \
             skipping desktop grant"
        );
    } else {
        grant_object_access(hdesk as HANDLE, &owned_sid, DESKTOP_FULL, "desktop");
    }

    Ok(())
}
