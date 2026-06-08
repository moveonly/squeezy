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
    let stem = temp_stem(path);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let nonce = randomish_nonce();
    // The 128-bit nonce plus pid is collision-proof in practice, so we skip
    // the `Path::exists()` probe that used to gate this call. `create_new(true)`
    // in `write_bytes_atomically` is itself atomic and will surface
    // `AlreadyExists` to the caller on the astronomically unlikely collision.
    parent.join(format!(".{stem}.{}.{nonce}.tmp", std::process::id()))
}

/// Build a stable, identifiable temp-file stem for `path`. Prefers the
/// file name's UTF-8 form so the temp visually matches the destination on
/// directory listings. Falls back to a hex encoding of the underlying
/// bytes when the file name is not UTF-8 (rare on Windows, possible on
/// Unix) so the temp still encodes the destination uniquely instead of
/// collapsing every non-UTF-8 name onto the synthetic `"squeezy-state"`
/// stem the previous implementation used.
fn temp_stem(path: &Path) -> String {
    let Some(name) = path.file_name() else {
        return "squeezy-state".to_string();
    };
    if let Some(text) = name.to_str() {
        return text.to_string();
    }
    let bytes = name.as_encoded_bytes();
    let mut out = String::with_capacity(bytes.len() * 2 + 9);
    out.push_str("squeezy-");
    for byte in bytes {
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{byte:02x}"));
    }
    out
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
    match getrandom::fill(&mut bytes) {
        Ok(()) => u128::from_le_bytes(bytes),
        Err(error) => {
            tracing::warn!(
                target: "squeezy::store",
                %error,
                "getrandom failed; falling back to system-time nanos for temp-path nonce",
            );
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        }
    }
}

#[cfg(windows)]
fn replace_file_windows(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    // MoveFileExW does not honour the long-path registry setting. For paths
    // longer than MAX_PATH, use the `\\?\` extended-length prefix. That prefix
    // requires (a) an absolute path and (b) only backslash separators — forward
    // slashes are not accepted.
    //
    // We work directly on the UTF-16 (`encode_wide`) representation rather than
    // round-tripping through `to_string_lossy` so that the rare non-UTF-8
    // (unpaired-surrogate) `OsString` segments Windows admits are preserved
    // verbatim instead of being silently replaced with U+FFFD before the
    // Win32 call.
    //
    // Three absolute-path shapes are recognised:
    //   * `\\?\...`         — already in extended form; pass through unchanged.
    //   * `\\server\share\` — UNC; rewritten to `\\?\UNC\server\share\` so the
    //                         long-path limit also lifts for network shares.
    //   * everything else absolute — get a plain `\\?\` prefix.
    fn wide(path: &Path) -> Vec<u16> {
        const BACKSLASH: u16 = b'\\' as u16;
        const QUESTION_MARK: u16 = b'?' as u16;

        let normalised: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .map(|c| if c == b'/' as u16 { BACKSLASH } else { c })
            .collect();
        let starts_with_double_backslash =
            normalised.len() >= 2 && normalised[0] == BACKSLASH && normalised[1] == BACKSLASH;
        let already_extended = starts_with_double_backslash
            && normalised.len() >= 4
            && normalised[2] == QUESTION_MARK
            && normalised[3] == BACKSLASH;

        let mut buf: Vec<u16> = Vec::with_capacity(normalised.len() + 8);
        if already_extended || !path.is_absolute() {
            buf.extend_from_slice(&normalised);
        } else if starts_with_double_backslash {
            // `\\server\share\...` -> `\\?\UNC\server\share\...`. The
            // `\\?\UNC\` prefix ends with a backslash, so skip the two
            // leading backslashes from `normalised` to avoid doubling them.
            buf.extend(r"\\?\UNC\".encode_utf16());
            buf.extend_from_slice(&normalised[2..]);
        } else {
            buf.extend(r"\\?\".encode_utf16());
            buf.extend_from_slice(&normalised);
        }
        buf.push(0);
        buf
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
