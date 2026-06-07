//! Local-account management: create / update / delete sandbox users, look up
//! SIDs, and hide/unhide accounts from the Windows logon screen.

use std::ffi::OsStr;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, HLOCAL, LocalFree,
};
use windows_sys::Win32::NetworkManagement::NetManagement::{
    NERR_Success, NERR_UserExists, NERR_UserNotFound, NetUserAdd, NetUserDel,
    NetUserSetInfo, UF_DONT_EXPIRE_PASSWD, UF_PASSWD_CANT_CHANGE, UF_SCRIPT, USER_INFO_1,
    USER_INFO_1003, USER_PRIV_USER,
};
use windows_sys::Win32::Security::{LookupAccountNameW, SID_NAME_USE};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE,
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW,
};

use super::winutil::{err, to_wide};

const USERLIST_KEY: &str =
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\SpecialAccounts\UserList";

/// Alphabet used for password generation: upper + lower + digits + symbols.
const PWD_CHARS: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*()-_=+";

// ── Password generation ───────────────────────────────────────────────────────

/// Generate a strong random password (≥24 chars) using `getrandom`.
pub(crate) fn generate_password() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("getrandom::fill failed");
    buf.iter()
        .map(|b| PWD_CHARS[(*b as usize) % PWD_CHARS.len()] as char)
        .collect()
}

// ── User create / update ──────────────────────────────────────────────────────

/// Create the local user `username` with `password`, or update their password
/// if they already exist.
pub(crate) fn create_or_update_user(username: &str, password: &str) -> crate::Result<()> {
    let name_w = to_wide(OsStr::new(username));
    let pwd_w = to_wide(OsStr::new(password));

    unsafe {
        let info = USER_INFO_1 {
            usri1_name: name_w.as_ptr() as *mut u16,
            usri1_password: pwd_w.as_ptr() as *mut u16,
            usri1_password_age: 0,
            usri1_priv: USER_PRIV_USER,
            usri1_home_dir: std::ptr::null_mut(),
            usri1_comment: std::ptr::null_mut(),
            usri1_flags: UF_SCRIPT | UF_DONT_EXPIRE_PASSWD | UF_PASSWD_CANT_CHANGE,
            usri1_script_path: std::ptr::null_mut(),
        };
        let status = NetUserAdd(
            std::ptr::null(),
            1,
            &info as *const _ as *mut u8,
            std::ptr::null_mut(),
        );
        if status == NERR_Success {
            return Ok(());
        }
        if status == NERR_UserExists {
            // User already exists — update password only.
            let pw_info = USER_INFO_1003 {
                usri1003_password: pwd_w.as_ptr() as *mut u16,
            };
            let upd = NetUserSetInfo(
                std::ptr::null(),
                name_w.as_ptr(),
                1003,
                &pw_info as *const _ as *mut u8,
                std::ptr::null_mut(),
            );
            if upd != NERR_Success {
                return Err(crate::WinSandboxError::win32(format!(
                    "NetUserSetInfo (password update) for '{}': code={upd}",
                    username
                )));
            }
            return Ok(());
        }
        Err(crate::WinSandboxError::win32(format!(
            "NetUserAdd for '{}': code={status}",
            username
        )))
    }
}

// ── User delete ───────────────────────────────────────────────────────────────

/// Delete the local user `username`.  Returns `Ok(false)` if the user was not
/// found; `Ok(true)` if successfully deleted.
pub(crate) fn delete_user(username: &str) -> crate::Result<bool> {
    let name_w = to_wide(OsStr::new(username));
    let status = unsafe { NetUserDel(std::ptr::null(), name_w.as_ptr()) };
    if status == NERR_Success {
        return Ok(true);
    }
    if status == NERR_UserNotFound || status == ERROR_ACCESS_DENIED {
        return Ok(false);
    }
    Err(crate::WinSandboxError::win32(format!(
        "NetUserDel for '{}': code={status}",
        username
    )))
}

// ── SID lookup ────────────────────────────────────────────────────────────────

/// Look up the local account SID for `username` and return it as a string like
/// `"S-1-5-21-…"`.  Uses `LookupAccountNameW` then `ConvertSidToStringSidW`.
pub(crate) fn account_sid_string(username: &str) -> crate::Result<String> {
    let name_w = to_wide(OsStr::new(username));

    // First call: size query.
    let mut sid_buf = vec![0u8; 68];
    let mut sid_len: u32 = sid_buf.len() as u32;
    let mut domain: Vec<u16> = vec![0u16; 256];
    let mut domain_len: u32 = domain.len() as u32;
    let mut use_type: SID_NAME_USE = 0;

    let ok = unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name_w.as_ptr(),
            sid_buf.as_mut_ptr() as *mut _,
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut use_type,
        )
    };
    if ok == 0 {
        // Retry with sizes returned by the first call.
        sid_buf.resize(sid_len as usize, 0);
        domain.resize(domain_len as usize, 0);
        let ok2 = unsafe {
            LookupAccountNameW(
                std::ptr::null(),
                name_w.as_ptr(),
                sid_buf.as_mut_ptr() as *mut _,
                &mut sid_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut use_type,
            )
        };
        if ok2 == 0 {
            return Err(err("LookupAccountNameW"));
        }
    }

    // Convert SID to string.
    let mut str_ptr: *mut u16 = std::ptr::null_mut();
    let ok = unsafe {
        ConvertSidToStringSidW(
            sid_buf.as_mut_ptr() as *mut _,
            &mut str_ptr,
        )
    };
    if ok == 0 || str_ptr.is_null() {
        return Err(err("ConvertSidToStringSidW"));
    }
    let sid_str = unsafe {
        let len = (0..).take_while(|&i| *str_ptr.add(i) != 0).count();
        let slice = std::slice::from_raw_parts(str_ptr, len);
        let s = String::from_utf16_lossy(slice);
        LocalFree(str_ptr as HLOCAL);
        s
    };
    Ok(sid_str)
}

// ── Hide / unhide from logon screen ──────────────────────────────────────────

/// Set `HKLM\...\SpecialAccounts\UserList\<username> = DWORD 0` to hide the
/// account from the Windows logon screen.
pub(crate) fn hide_user_from_login(username: &str) -> crate::Result<()> {
    let key = open_or_create_userlist_key()?;
    let name_w = to_wide(OsStr::new(username));
    let value: u32 = 0;
    let status = unsafe {
        RegSetValueExW(
            key,
            name_w.as_ptr(),
            0,
            REG_DWORD,
            &value as *const u32 as *const u8,
            std::mem::size_of_val(&value) as u32,
        )
    };
    unsafe { RegCloseKey(key) };
    if status != 0 {
        return Err(crate::WinSandboxError::win32(format!(
            "RegSetValueExW (hide user '{}') failed: code={status}",
            username
        )));
    }
    Ok(())
}

/// Delete `HKLM\...\SpecialAccounts\UserList\<username>` (unhide from logon
/// screen, best-effort).
pub(crate) fn unhide_user_from_login(username: &str) {
    let key = match open_userlist_key_for_write() {
        Ok(k) => k,
        Err(_) => return,
    };
    let name_w = to_wide(OsStr::new(username));
    unsafe {
        RegDeleteValueW(key, name_w.as_ptr());
        RegCloseKey(key);
    }
}

// ── Registry helpers ──────────────────────────────────────────────────────────

fn open_or_create_userlist_key() -> crate::Result<HKEY> {
    let key_w = to_wide(USERLIST_KEY);
    let mut hkey: HKEY = std::ptr::null_mut();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            key_w.as_ptr(),
            0,
            std::ptr::null_mut(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            std::ptr::null_mut(),
            &mut hkey,
            std::ptr::null_mut(),
        )
    };
    if status != 0 {
        return Err(crate::WinSandboxError::win32(format!(
            "RegCreateKeyExW (UserList) failed: code={status}"
        )));
    }
    Ok(hkey)
}

fn open_userlist_key_for_write() -> crate::Result<HKEY> {
    let key_w = to_wide(USERLIST_KEY);
    let mut hkey: HKEY = std::ptr::null_mut();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            key_w.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        )
    };
    if status != 0 {
        return Err(crate::WinSandboxError::win32(format!(
            "RegOpenKeyExW (UserList) failed: code={status}"
        )));
    }
    Ok(hkey)
}
