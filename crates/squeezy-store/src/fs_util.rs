use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use squeezy_core::{Result, SqueezyError};

pub(crate) fn write_json_atomically(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(json_error)?;
    write_bytes_atomically(path, &bytes).map_err(annotate_replace_error)
}

pub(crate) fn write_bytes_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = unique_temp_path(path);
    {
        let mut file = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    if let Err(error) = replace_file(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

pub(crate) fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        replace_file_windows(from, to)
    }
    #[cfg(not(windows))]
    {
        fs::rename(from, to)
    }
}

pub(crate) fn move_path(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to)
        .map_err(|error| annotate_move_error("move", from, to, error))
        .map_err(SqueezyError::Io)
}

pub(crate) fn rotate_file(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to)
        .map_err(|error| annotate_move_error("rotate", from, to, error))
        .map_err(SqueezyError::Io)
}

pub(crate) fn remove_file(path: &Path) -> Result<()> {
    fs::remove_file(path)
        .map_err(|error| annotate_single_path_error("remove", path, error))
        .map_err(SqueezyError::Io)
}

pub(crate) fn user_squeezy_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        return Some(PathBuf::from(home).join(".squeezy"));
    }
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return Some(PathBuf::from(appdata).join("Squeezy"));
        }
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return Some(PathBuf::from(profile).join(".squeezy"));
        }
    }
    None
}

pub fn user_squeezy_dir_detail() -> String {
    match user_squeezy_dir() {
        Some(path) => format!("user_global={}", path.display()),
        None => {
            #[cfg(windows)]
            {
                "user_global unavailable: HOME, APPDATA, and USERPROFILE are unset".to_string()
            }
            #[cfg(not(windows))]
            {
                "user_global unavailable: HOME is unset".to_string()
            }
        }
    }
}

pub fn windows_storage_hint(error: &dyn std::fmt::Display) -> String {
    if cfg!(windows) {
        format!(
            "{error}; on Windows this usually means the file is in use by another Squeezy process, editor, sync client, antivirus scanner, or indexer"
        )
    } else {
        error.to_string()
    }
}

pub(crate) fn unique_temp_path(path: &Path) -> PathBuf {
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("squeezy-state");
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    for attempt in 0..32u32 {
        let nonce = randomish_nonce();
        let tmp = parent.join(format!(
            ".{stem}.{}.{}.{}.tmp",
            std::process::id(),
            nonce,
            attempt
        ));
        if !tmp.exists() {
            return tmp;
        }
    }
    parent.join(format!(
        ".{stem}.{}.{}.tmp",
        std::process::id(),
        randomish_nonce()
    ))
}

fn annotate_replace_error(error: io::Error) -> SqueezyError {
    SqueezyError::Io(io::Error::new(
        error.kind(),
        windows_storage_hint(&format!("replace failed: {error}")),
    ))
}

fn annotate_move_error(action: &str, from: &Path, to: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        windows_storage_hint(&format!(
            "{action} failed from {} to {}: {error}",
            from.display(),
            to.display()
        )),
    )
}

fn annotate_single_path_error(action: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        windows_storage_hint(&format!("{action} failed for {}: {error}", path.display())),
    )
}

fn json_error(error: serde_json::Error) -> SqueezyError {
    SqueezyError::Agent(format!("json serialization failed: {error}"))
}

fn randomish_nonce() -> u128 {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_ok() {
        return u128::from_le_bytes(bytes);
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

#[cfg(windows)]
fn replace_file_windows(from: &Path, to: &Path) -> io::Result<()> {
    use std::{ffi::OsString, iter, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    // MoveFileExW does not honour the long-path registry setting.  For paths
    // longer than MAX_PATH, use the \\?\ extended-length prefix.  That prefix
    // requires (a) an absolute path and (b) only backslash separators — forward
    // slashes are not accepted.  Normalise any forward slashes before prefixing.
    fn wide(path: &Path) -> Vec<u16> {
        let as_str = path.as_os_str().to_string_lossy();
        // Normalise forward slashes → backslashes required by \\?\ paths.
        let normalised = as_str.replace('/', "\\");
        let extended: OsString = if path.is_absolute() && !normalised.starts_with(r"\\") {
            OsString::from(format!(r"\\?\{normalised}"))
        } else {
            OsString::from(normalised.as_ref())
        };
        extended.encode_wide().chain(iter::once(0)).collect()
    }

    let from = wide(from);
    let to = wide(to);
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    let ok = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
