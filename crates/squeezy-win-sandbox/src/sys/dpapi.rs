//! DPAPI wrappers: machine-scope protect/unprotect.
//!
//! Uses `CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN` so that both
//! the elevated setup process and the unelevated agent process (running as the
//! real user) can decrypt the same blob.

use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    CryptUnprotectData,
};

use crate::WinSandboxError;

fn make_blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    }
}

/// Encrypt `plaintext` with DPAPI at machine scope.
pub(crate) fn protect(plaintext: &[u8]) -> crate::Result<Vec<u8>> {
    let in_blob = make_blob(plaintext);
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &in_blob,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN | CRYPTPROTECT_LOCAL_MACHINE,
            &mut out_blob,
        )
    };
    if ok == 0 {
        return Err(WinSandboxError::win32(super::winutil::format_win_error(
            "CryptProtectData",
        )));
    }
    let result =
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) }.to_vec();
    unsafe {
        if !out_blob.pbData.is_null() {
            LocalFree(out_blob.pbData as HLOCAL);
        }
    }
    Ok(result)
}

/// Decrypt a blob produced by [`protect`].
pub(crate) fn unprotect(ciphertext: &[u8]) -> crate::Result<Vec<u8>> {
    let in_blob = make_blob(ciphertext);
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &in_blob,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN | CRYPTPROTECT_LOCAL_MACHINE,
            &mut out_blob,
        )
    };
    if ok == 0 {
        return Err(WinSandboxError::win32(super::winutil::format_win_error(
            "CryptUnprotectData",
        )));
    }
    let result =
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) }.to_vec();
    unsafe {
        if !out_blob.pbData.is_null() {
            LocalFree(out_blob.pbData as HLOCAL);
        }
    }
    Ok(result)
}
