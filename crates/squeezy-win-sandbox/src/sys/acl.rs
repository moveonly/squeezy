//! On-disk DACL editing: add allow / deny-write / deny-read ACEs.

use std::os::windows::fs::MetadataExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HLOCAL, LocalFree};
use windows_sys::Win32::Security::ACL;
use windows_sys::Win32::Security::Authorization::{
    ACCESS_MODE, DENY_ACCESS, EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, SE_FILE_OBJECT, SET_ACCESS,
    SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, NO_INHERITANCE, PSID};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_APPEND_DATA, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD,
    FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_WRITE_ATTRIBUTES,
    FILE_WRITE_DATA, FILE_WRITE_EA,
};

use super::winutil::{OwnedSid, to_wide_path};

const CONTAINER_INHERIT_ACE: u32 = 0x2;
const OBJECT_INHERIT_ACE: u32 = 0x1;
const INHERIT_FLAGS: u32 = CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE;

/// Core: fetch the existing DACL, merge one new ACE, and write it back.
///
/// `inherit = true` sets `CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE`.
fn apply_ace(
    path: &Path,
    sid_str: &str,
    access_mask: u32,
    mode: ACCESS_MODE,
    inherit: bool,
) -> crate::Result<()> {
    let owned = OwnedSid::from_str(sid_str)?;
    let psid: PSID = owned.as_psid();

    let path_wide = to_wide_path(path);
    let mut p_sd = std::ptr::null_mut();
    let mut p_dacl: *mut ACL = std::ptr::null_mut();

    let code = unsafe {
        GetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut p_dacl,
            std::ptr::null_mut(),
            &mut p_sd,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(crate::WinSandboxError::win32(format!(
            "GetNamedSecurityInfoW on '{}': code={code}",
            path.display()
        )));
    }

    let inheritance = if inherit {
        INHERIT_FLAGS
    } else {
        NO_INHERITANCE
    };

    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: access_mask,
        grfAccessMode: mode,
        grfInheritance: inheritance,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: psid as *mut u16,
        },
    };

    let mut p_new_dacl: *mut ACL = std::ptr::null_mut();
    let code2 = unsafe { SetEntriesInAclW(1, &ea, p_dacl, &mut p_new_dacl) };

    if code2 != ERROR_SUCCESS {
        unsafe {
            if !p_sd.is_null() {
                LocalFree(p_sd as HLOCAL);
            }
        }
        return Err(crate::WinSandboxError::win32(format!(
            "SetEntriesInAclW on '{}': code={code2}",
            path.display()
        )));
    }

    // path_wide was already computed; build a mutable copy for SetNamedSecurityInfoW.
    let mut path_wide2 = to_wide_path(path);
    let code3 = unsafe {
        SetNamedSecurityInfoW(
            path_wide2.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            p_new_dacl,
            std::ptr::null_mut(),
        )
    };

    unsafe {
        if !p_new_dacl.is_null() {
            LocalFree(p_new_dacl as HLOCAL);
        }
        if !p_sd.is_null() {
            LocalFree(p_sd as HLOCAL);
        }
    }

    if code3 != ERROR_SUCCESS {
        return Err(crate::WinSandboxError::win32(format!(
            "SetNamedSecurityInfoW on '{}': code={code3}",
            path.display()
        )));
    }
    Ok(())
}

/// True for any entry whose attributes include `FILE_ATTRIBUTE_REPARSE_POINT`.
///
/// `std::fs::FileType::is_symlink` returns `false` for NTFS junctions
/// (`mklink /J`), AppExec stubs, and OneDrive cloud reparse points: those
/// surface as `is_dir() = true` to Rust even though following them takes us
/// outside the workspace boundary. Granting capability ACEs through such an
/// entry would be a sandbox escape, so the recursion must stop at every
/// reparse point regardless of its specific tag.
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Grant read/write/execute/delete rights (inheritable) to `sid_str` on `path`.
#[allow(dead_code)]
pub(crate) fn add_allow_ace(path: &Path, sid_str: &str) -> crate::Result<()> {
    let mask =
        FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE | FILE_DELETE_CHILD;
    apply_ace(path, sid_str, mask, SET_ACCESS, true)
}

fn apply_ace_recursive(
    path: &Path,
    sid_str: &str,
    access_mask: u32,
    mode: ACCESS_MODE,
) -> crate::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if is_reparse_point(&metadata) {
        // Apply the ACE to the reparse point itself but never traverse into
        // the target — the target may resolve to an arbitrary host path.
        apply_ace(path, sid_str, access_mask, mode, false)?;
        return Ok(());
    }
    apply_ace(
        path,
        sid_str,
        access_mask,
        mode,
        metadata.file_type().is_dir(),
    )?;

    if !metadata.file_type().is_dir() {
        return Ok(());
    }

    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let child_metadata = entry.metadata()?;
        if is_reparse_point(&child_metadata) {
            continue;
        }
        apply_ace_recursive(&entry.path(), sid_str, access_mask, mode)?;
    }
    Ok(())
}

/// Grant read/write/execute on `path` and every existing non-symlink descendant.
pub(crate) fn add_allow_ace_recursive(path: &Path, sid_str: &str) -> crate::Result<()> {
    let mask =
        FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE | FILE_DELETE_CHILD;
    apply_ace_recursive(path, sid_str, mask, SET_ACCESS)
}

/// Add an inheritable deny ACE blocking all write/delete operations.
pub(crate) fn add_deny_write_ace(path: &Path, sid_str: &str) -> crate::Result<()> {
    let mask = FILE_GENERIC_WRITE
        | FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_EA
        | FILE_WRITE_ATTRIBUTES
        | DELETE
        | FILE_DELETE_CHILD;
    apply_ace(path, sid_str, mask, DENY_ACCESS, true)
}

/// Add a deny-write ACE on `path` and every existing non-symlink descendant.
pub(crate) fn add_deny_write_ace_recursive(path: &Path, sid_str: &str) -> crate::Result<()> {
    let mask = FILE_GENERIC_WRITE
        | FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_EA
        | FILE_WRITE_ATTRIBUTES
        | DELETE
        | FILE_DELETE_CHILD;
    apply_ace_recursive(path, sid_str, mask, DENY_ACCESS)
}

/// Add a deny ACE blocking writes to this object without inheriting to children.
///
/// The world-writable escape audit uses this for ancestor directories like
/// `%TEMP%`: denying the directory object blocks new entries there without
/// poisoning explicitly allowed writable roots beneath it through inheritance.
pub(crate) fn add_deny_write_ace_no_inherit(path: &Path, sid_str: &str) -> crate::Result<()> {
    let mask = FILE_GENERIC_WRITE
        | FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_EA
        | FILE_WRITE_ATTRIBUTES
        | DELETE
        | FILE_DELETE_CHILD;
    apply_ace(path, sid_str, mask, DENY_ACCESS, false)
}

/// Add an inheritable deny ACE blocking all read operations.
#[allow(dead_code)]
pub(crate) fn add_deny_read_ace(path: &Path, sid_str: &str) -> crate::Result<()> {
    apply_ace(path, sid_str, FILE_GENERIC_READ, DENY_ACCESS, true)
}

/// Add a deny-read ACE on `path` and every existing non-symlink descendant.
pub(crate) fn add_deny_read_ace_recursive(path: &Path, sid_str: &str) -> crate::Result<()> {
    apply_ace_recursive(path, sid_str, FILE_GENERIC_READ, DENY_ACCESS)
}

/// Grant read/execute on `path` and every existing non-symlink descendant.
pub(crate) fn add_allow_read_ace_recursive(path: &Path, sid_str: &str) -> crate::Result<()> {
    let mask = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
    apply_ace_recursive(path, sid_str, mask, SET_ACCESS)
}
