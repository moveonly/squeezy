//! RAII wrapper around `PROC_THREAD_ATTRIBUTE_LIST` for scoping handle
//! inheritance to an exact set of handles via `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
//!
//! Usage:
//! 1. Call [`ProcThreadAttrList::new`] with `attr_count = 1`.
//! 2. Call [`ProcThreadAttrList::set_handle_list`] with a slice of the handles
//!    to be inherited (e.g. `[stdin_read, stdout_write, stderr_write]`).  The
//!    handles are stored inside the struct so the pointer passed to Win32 stays
//!    valid for the lifetime of the struct.
//! 3. Pass [`ProcThreadAttrList::as_mut_ptr`] as `lpAttributeList` in
//!    `STARTUPINFOEXW`, keeping the struct alive past the `CreateProcess*` call.
//! 4. The attribute list is deleted and the buffer freed when the struct drops.

use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
use windows_sys::Win32::System::Threading::{
    DeleteProcThreadAttributeList, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, UpdateProcThreadAttribute,
};

use crate::WinSandboxError;

/// RAII owner of a `PROC_THREAD_ATTRIBUTE_LIST` buffer.
///
/// The `handle_list` field keeps the handle array alive across the
/// `CreateProcessAsUserW` call; `lpAttributeList` points into `buffer`, which
/// the Win32 attribute list in turn references via `handle_list`.
pub(crate) struct ProcThreadAttrList {
    /// Opaque buffer that holds the initialized attribute list.
    buffer: Vec<u8>,
    /// The exact handles that will be inherited by the child.  Stored here so
    /// the slice pointer passed to `UpdateProcThreadAttribute` remains valid.
    handle_list: Vec<HANDLE>,
}

impl ProcThreadAttrList {
    /// Allocate and initialise an attribute list with room for `attr_count`
    /// attributes.
    pub(crate) fn new(attr_count: u32) -> crate::Result<Self> {
        // First call: obtain required buffer size.
        let mut size: usize = 0;
        unsafe {
            // Passing a null pointer is the documented way to query the size;
            // the function will return FALSE but populates `size`.
            InitializeProcThreadAttributeList(std::ptr::null_mut(), attr_count, 0, &mut size);
        }
        if size == 0 {
            return Err(WinSandboxError::win32(format!(
                "InitializeProcThreadAttributeList (size query) failed: code={}",
                unsafe { GetLastError() }
            )));
        }

        // Second call: initialise the list in the allocated buffer.
        let mut buffer = vec![0u8; size];
        let list = buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        let ok = unsafe { InitializeProcThreadAttributeList(list, attr_count, 0, &mut size) };
        if ok == 0 {
            return Err(WinSandboxError::win32(format!(
                "InitializeProcThreadAttributeList failed: code={}",
                unsafe { GetLastError() }
            )));
        }

        Ok(Self {
            buffer,
            handle_list: Vec::new(),
        })
    }

    /// Return the raw pointer to the attribute list (suitable for
    /// `STARTUPINFOEXW::lpAttributeList`).
    pub(crate) fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST
    }

    /// Register exactly the given handles as the `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`
    /// attribute.  The handles are copied into `self.handle_list` so the pointer
    /// that Win32 holds into them remains valid for the lifetime of `self`.
    ///
    /// Must be called before the `CreateProcess*` call and at most once per
    /// instance (calling it again would invalidate the previously registered
    /// pointer — the Win32 docs say the list is immutable after
    /// `CreateProcess`).
    pub(crate) fn set_handle_list(&mut self, handles: &[HANDLE]) -> crate::Result<()> {
        // Store handles so that `self.handle_list.as_mut_ptr()` remains valid.
        self.handle_list = handles.to_vec();

        let list = self.as_mut_ptr();
        let ok = unsafe {
            UpdateProcThreadAttribute(
                list,
                0, // dwFlags — reserved, must be 0
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                self.handle_list.as_mut_ptr().cast(),
                std::mem::size_of_val(self.handle_list.as_slice()),
                std::ptr::null_mut(), // lpPreviousValue — not used
                std::ptr::null(),     // lpReturnSize — not used
            )
        };
        if ok == 0 {
            return Err(WinSandboxError::win32(format!(
                "UpdateProcThreadAttribute(HANDLE_LIST) failed: code={}",
                unsafe { GetLastError() }
            )));
        }
        Ok(())
    }
}

impl Drop for ProcThreadAttrList {
    fn drop(&mut self) {
        // SAFETY: `buffer` was successfully initialised in `new`; we call
        // `DeleteProcThreadAttributeList` exactly once here.
        unsafe {
            DeleteProcThreadAttributeList(self.as_mut_ptr());
        }
    }
}
