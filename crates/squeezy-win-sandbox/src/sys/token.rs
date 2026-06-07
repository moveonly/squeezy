//! Restricted-token creation via `CreateRestrictedToken`.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{GetLastError, HANDLE, HLOCAL, LUID, LocalFree};
use windows_sys::Win32::Security::ACL;
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, CopySid, CreateRestrictedToken, GetLengthSid, GetTokenInformation,
    LookupPrivilegeValueW, PSID, SID_AND_ATTRIBUTES, SetTokenInformation, TOKEN_ADJUST_DEFAULT,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_PRIVILEGES, TOKEN_QUERY,
    TokenDefaultDacl, TokenGroups,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::winutil::{self, OwnedSid, ScopedHandle, SidBuf};

// ── Constants ─────────────────────────────────────────────────────────────────

const DISABLE_MAX_PRIVILEGE: u32 = 0x1;
const LUA_TOKEN: u32 = 0x4;
const WRITE_RESTRICTED: u32 = 0x8;
const GENERIC_ALL: u32 = 0x1000_0000;
const SE_GROUP_LOGON_ID: u32 = 0xC000_0000;
const SE_PRIVILEGE_ENABLED: u32 = 0x0000_0002;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Open the current process token with the rights needed for restriction.
pub(crate) fn current_process_token() -> crate::Result<ScopedHandle> {
    let desired = TOKEN_DUPLICATE
        | TOKEN_QUERY
        | TOKEN_ASSIGN_PRIMARY
        | TOKEN_ADJUST_DEFAULT
        | TOKEN_ADJUST_PRIVILEGES;
    let mut h: HANDLE = std::ptr::null_mut();
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), desired, &mut h) };
    if ok == 0 {
        return Err(winutil::err("OpenProcessToken"));
    }
    Ok(ScopedHandle::new(h))
}

/// Find the logon SID in the token's group list and copy it into a byte vec.
fn logon_sid(token: HANDLE) -> crate::Result<Vec<u8>> {
    // First call: get required buffer size.
    let mut needed: u32 = 0;
    unsafe {
        GetTokenInformation(token, TokenGroups, std::ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        return Err(winutil::err("GetTokenInformation(TokenGroups) size"));
    }
    let mut buf = vec![0u8; needed as usize];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenGroups,
            buf.as_mut_ptr() as *mut c_void,
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(winutil::err("GetTokenInformation(TokenGroups)"));
    }

    // TOKEN_GROUPS layout: DWORD GroupCount; SID_AND_ATTRIBUTES Groups[];
    // On 64-bit the Groups array is pointer-aligned after the 4-byte count.
    let group_count = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const u32) } as usize;
    let after_count = unsafe { buf.as_ptr().add(std::mem::size_of::<u32>()) } as usize;
    let align = std::mem::align_of::<SID_AND_ATTRIBUTES>();
    let groups_ptr = ((after_count + align - 1) & !(align - 1)) as *const SID_AND_ATTRIBUTES;

    for i in 0..group_count {
        let entry = unsafe { std::ptr::read_unaligned(groups_ptr.add(i)) };
        if (entry.Attributes & SE_GROUP_LOGON_ID) == SE_GROUP_LOGON_ID {
            let sid_len = unsafe { GetLengthSid(entry.Sid) };
            if sid_len == 0 {
                return Err(winutil::err("GetLengthSid(logon SID)"));
            }
            let mut out = vec![0u8; sid_len as usize];
            let ok2 = unsafe { CopySid(sid_len, out.as_mut_ptr() as PSID, entry.Sid) };
            if ok2 == 0 {
                return Err(winutil::err("CopySid(logon SID)"));
            }
            return Ok(out);
        }
    }
    Err(crate::WinSandboxError::win32(
        "logon SID not found in token groups".to_string(),
    ))
}

/// Build and set a permissive default DACL on `token` so the sandboxed process
/// can create named pipes and other kernel objects.
fn set_permissive_default_dacl(token: HANDLE, dacl_sids: &[PSID]) -> crate::Result<()> {
    if dacl_sids.is_empty() {
        return Ok(());
    }

    let entries: Vec<EXPLICIT_ACCESS_W> = dacl_sids
        .iter()
        .map(|&sid| EXPLICIT_ACCESS_W {
            grfAccessPermissions: GENERIC_ALL,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: 0,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: sid as *mut u16,
            },
        })
        .collect();

    let mut p_new_dacl: *mut ACL = std::ptr::null_mut();
    let res = unsafe {
        SetEntriesInAclW(
            entries.len() as u32,
            entries.as_ptr(),
            std::ptr::null_mut(),
            &mut p_new_dacl,
        )
    };
    if res != 0 {
        // SetEntriesInAclW returns ERROR_SUCCESS (0) on success, non-zero on failure.
        return Err(crate::WinSandboxError::win32(format!(
            "SetEntriesInAclW for default DACL failed: {res}"
        )));
    }

    // TokenDefaultDacl info struct: a single pointer to ACL.
    #[repr(C)]
    struct TokenDefaultDaclInfo {
        default_dacl: *mut ACL,
    }
    let mut info = TokenDefaultDaclInfo {
        default_dacl: p_new_dacl,
    };
    let ok = unsafe {
        SetTokenInformation(
            token,
            TokenDefaultDacl,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<TokenDefaultDaclInfo>() as u32,
        )
    };
    unsafe {
        if !p_new_dacl.is_null() {
            LocalFree(p_new_dacl as HLOCAL);
        }
    }
    if ok == 0 {
        return Err(winutil::err("SetTokenInformation(TokenDefaultDacl)"));
    }
    Ok(())
}

/// Enable `SeChangeNotifyPrivilege` on the token so the sandboxed process
/// can traverse directory trees it has access to.
fn enable_change_notify(token: HANDLE) -> crate::Result<()> {
    let name = winutil::to_wide("SeChangeNotifyPrivilege");
    let mut luid = LUID {
        LowPart: 0,
        HighPart: 0,
    };
    let ok = unsafe { LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) };
    if ok == 0 {
        return Err(winutil::err(
            "LookupPrivilegeValueW(SeChangeNotifyPrivilege)",
        ));
    }

    let mut tp: TOKEN_PRIVILEGES = unsafe { std::mem::zeroed() };
    tp.PrivilegeCount = 1;
    tp.Privileges[0].Luid = luid;
    tp.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;

    let ok2 = unsafe {
        AdjustTokenPrivileges(token, 0, &tp, 0, std::ptr::null_mut(), std::ptr::null_mut())
    };
    if ok2 == 0 {
        return Err(winutil::err("AdjustTokenPrivileges"));
    }
    // `AdjustTokenPrivileges` returns success even when a privilege could not be
    // assigned, reporting `ERROR_NOT_ALL_ASSIGNED` (1300) via GetLastError. That
    // is benign here: `SeChangeNotifyPrivilege` is only a traversal convenience,
    // and `DISABLE_MAX_PRIVILEGE` may already have stripped it from the
    // restricted token so it cannot be re-enabled. Treat any leftover error as
    // non-fatal — failing the whole spawn over a missing traverse privilege
    // would be worse than running without it.
    let gle = unsafe { GetLastError() };
    if gle != 0 {
        tracing::debug!(
            gle,
            "AdjustTokenPrivileges(SeChangeNotifyPrivilege) not fully assigned; continuing"
        );
    }
    Ok(())
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Create a `WRITE_RESTRICTED | LUA_TOKEN | DISABLE_MAX_PRIVILEGE` token whose
/// restricting-SID list is `[cap_sids..., logon, world]`.
///
/// A permissive default DACL (GENERIC_ALL for logon + world + caps) is set so
/// the sandboxed process can create pipes.  `SeChangeNotifyPrivilege` is
/// re-enabled so directory traversal works.
pub(crate) fn create_sandbox_token(cap_sid_strings: &[String]) -> crate::Result<ScopedHandle> {
    // Convert string SIDs to owned SIDs; keep them alive across the call.
    let cap_owned: Vec<OwnedSid> = cap_sid_strings
        .iter()
        .map(|s| OwnedSid::from_str(s))
        .collect::<crate::Result<_>>()?;

    let base = current_process_token()?;

    // Logon SID
    let logon_bytes = logon_sid(base.as_raw())?;
    let psid_logon = logon_bytes.as_ptr() as PSID;

    // World SID
    let mut world_buf = SidBuf::zeroed();
    world_buf.init_world_sid()?;
    let psid_world = world_buf.as_psid();

    // Build restricting-SID array: caps..., logon, world
    let mut restricting: Vec<SID_AND_ATTRIBUTES> = Vec::with_capacity(cap_owned.len() + 2);
    for co in &cap_owned {
        restricting.push(SID_AND_ATTRIBUTES {
            Sid: co.as_psid(),
            Attributes: 0,
        });
    }
    restricting.push(SID_AND_ATTRIBUTES {
        Sid: psid_logon,
        Attributes: 0,
    });
    restricting.push(SID_AND_ATTRIBUTES {
        Sid: psid_world,
        Attributes: 0,
    });

    let mut new_token: HANDLE = std::ptr::null_mut();
    let flags = DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED;
    let ok = unsafe {
        CreateRestrictedToken(
            base.as_raw(),
            flags,
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            restricting.len() as u32,
            restricting.as_mut_ptr(),
            &mut new_token,
        )
    };
    if ok == 0 {
        return Err(winutil::err("CreateRestrictedToken"));
    }
    let token = ScopedHandle::new(new_token);

    // Set permissive default DACL: logon + world + caps
    let mut dacl_sids: Vec<PSID> = Vec::with_capacity(cap_owned.len() + 2);
    dacl_sids.push(psid_logon);
    dacl_sids.push(psid_world);
    for co in &cap_owned {
        dacl_sids.push(co.as_psid());
    }
    set_permissive_default_dacl(token.as_raw(), &dacl_sids)?;

    // Re-enable SeChangeNotifyPrivilege.
    enable_change_notify(token.as_raw())?;

    Ok(token)
}
