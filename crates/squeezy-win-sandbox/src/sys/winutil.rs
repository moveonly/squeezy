//! Foundation utilities: handle RAII, SID ownership, string conversion,
//! error formatting, command-line building, and environment-block construction.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::{CreateWellKnownSid, PSID, WinWorldSid};
use windows_sys::Win32::System::Diagnostics::Debug::{
    FORMAT_MESSAGE_ALLOCATE_BUFFER, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS,
    FormatMessageW,
};

use crate::WinSandboxError;

// ── ScopedHandle ─────────────────────────────────────────────────────────────

/// RAII wrapper for a Win32 `HANDLE` that calls `CloseHandle` on drop.
pub(crate) struct ScopedHandle(HANDLE);

impl ScopedHandle {
    /// Wrap a raw handle.  A null or `INVALID_HANDLE_VALUE` handle is still
    /// wrapped — `is_valid()` will return false and drop will not close it.
    pub(crate) fn new(h: HANDLE) -> Self {
        Self(h)
    }

    /// Borrow the raw handle.
    pub(crate) fn as_raw(&self) -> HANDLE {
        self.0
    }

    /// Consume the wrapper without closing the handle (leaks ownership to
    /// the caller).
    pub(crate) fn into_raw(self) -> HANDLE {
        let h = self.0;
        std::mem::forget(self);
        h
    }

    /// Returns `true` when the handle is neither null nor `INVALID_HANDLE_VALUE`.
    pub(crate) fn is_valid(&self) -> bool {
        !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE
    }
}

impl Drop for ScopedHandle {
    fn drop(&mut self) {
        if self.is_valid() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

// ── OwnedSid ─────────────────────────────────────────────────────────────────

/// Owns a SID allocated by `LocalAlloc` / `ConvertStringSidToSidW`.
/// Freed with `LocalFree` on drop.
pub(crate) struct OwnedSid(PSID);

impl OwnedSid {
    /// Convert a string SID (e.g. `"S-1-5-21-…"`) into an `OwnedSid`.
    pub(crate) fn from_str(sid_str: &str) -> crate::Result<Self> {
        let wide = to_wide(sid_str);
        let mut psid: PSID = std::ptr::null_mut();
        let ok = unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut psid) };
        if ok == 0 || psid.is_null() {
            return Err(err("ConvertStringSidToSidW"));
        }
        Ok(Self(psid))
    }

    pub(crate) fn as_psid(&self) -> PSID {
        self.0
    }
}

impl Drop for OwnedSid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0 as HLOCAL);
            }
        }
    }
}

// ── SidBuf ───────────────────────────────────────────────────────────────────

/// Stack buffer large enough for any SID (`SECURITY_MAX_SID_SIZE` = 68 bytes).
/// Use with `CreateWellKnownSid`.
pub(crate) struct SidBuf([u8; 68]);

impl SidBuf {
    pub(crate) fn zeroed() -> Self {
        Self([0u8; 68])
    }

    pub(crate) fn as_psid(&mut self) -> PSID {
        self.0.as_mut_ptr() as PSID
    }

    /// Fill with the World (Everyone) SID.
    pub(crate) fn init_world_sid(&mut self) -> crate::Result<()> {
        let mut size: u32 = self.0.len() as u32;
        let ok = unsafe {
            CreateWellKnownSid(WinWorldSid, std::ptr::null_mut(), self.as_psid(), &mut size)
        };
        if ok == 0 {
            return Err(err("CreateWellKnownSid"));
        }
        Ok(())
    }
}

// ── String / path helpers ─────────────────────────────────────────────────────

/// Encode a string as a NUL-terminated UTF-16 vector.
pub(crate) fn to_wide<S: AsRef<OsStr>>(s: S) -> Vec<u16> {
    let mut v: Vec<u16> = s.as_ref().encode_wide().collect();
    v.push(0);
    v
}

/// Encode a `Path` as a NUL-terminated UTF-16 vector.
pub(crate) fn to_wide_path(p: &Path) -> Vec<u16> {
    to_wide(p)
}

// ── Error helpers ─────────────────────────────────────────────────────────────

/// Return the current `GetLastError` value.
pub(crate) fn last_error_code() -> u32 {
    unsafe { GetLastError() }
}

/// Format a Win32 error as `"<context>: code=<n> <FormatMessageW text>"`.
pub(crate) fn format_win_error(context: &str) -> String {
    let code = last_error_code();
    let msg = unsafe {
        let mut buf_ptr: *mut u16 = std::ptr::null_mut();
        let flags = FORMAT_MESSAGE_ALLOCATE_BUFFER
            | FORMAT_MESSAGE_FROM_SYSTEM
            | FORMAT_MESSAGE_IGNORE_INSERTS;
        let len = FormatMessageW(
            flags,
            std::ptr::null(),
            code,
            0,
            (&raw mut buf_ptr) as *mut u16,
            0,
            std::ptr::null_mut(),
        );
        if len == 0 || buf_ptr.is_null() {
            format!("code={code}")
        } else {
            let slice = std::slice::from_raw_parts(buf_ptr, len as usize);
            let s = String::from_utf16_lossy(slice).trim().to_string();
            LocalFree(buf_ptr as HLOCAL);
            format!("code={code} {s}")
        }
    };
    format!("{context}: {msg}")
}

/// Build and return a `WinSandboxError::Win32` whose message is the result of
/// `format_win_error(context)`.
pub(crate) fn err(context: &str) -> WinSandboxError {
    WinSandboxError::win32(format_win_error(context))
}

// ── Command-line building ─────────────────────────────────────────────────────

/// Quote a single argument following Windows `CommandLineToArgvW` rules.
fn quote_arg(arg: &str) -> String {
    let needs_quote = arg.is_empty()
        || arg
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '"'));

    if !needs_quote {
        return arg.to_string();
    }

    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes: usize = 0;

    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                // Escape all pending backslashes then the quote.
                for _ in 0..backslashes * 2 + 1 {
                    out.push('\\');
                }
                out.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    // Trailing backslashes before the closing quote must be doubled.
    for _ in 0..backslashes * 2 {
        out.push('\\');
    }
    out.push('"');
    out
}

/// Build a NUL-terminated wide command line for `CreateProcess*`.
pub(crate) fn build_command_line(argv: &[String]) -> Vec<u16> {
    let line = argv
        .iter()
        .map(|a| quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ");
    to_wide(&line)
}

// ── Environment-block construction ────────────────────────────────────────────

/// Build a double-NUL-terminated wide environment block sorted case-insensitively.
pub(crate) fn make_env_block(env: &HashMap<String, String>) -> Vec<u16> {
    let mut pairs: Vec<(&String, &String)> = env.iter().collect();
    pairs.sort_by(|(a, _), (b, _)| a.to_uppercase().cmp(&b.to_uppercase()).then(a.cmp(b)));

    let mut block: Vec<u16> = Vec::new();
    for (k, v) in pairs {
        let entry = format!("{k}={v}");
        let wide: Vec<u16> = OsStr::new(&entry).encode_wide().collect();
        block.extend_from_slice(&wide);
        block.push(0);
    }
    block.push(0); // double-NUL terminator
    block
}
