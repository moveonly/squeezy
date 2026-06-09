//! Escape-vector audit: detect world-writable directories that could allow the
//! sandboxed process (which has the World SID as a restricting SID) to escape
//! its write confinement, then deny-write the cap SID on each such directory.
//!
//! Security model
//! ──────────────
//! The restricted token's restricting-SID list is `[cap SIDs…, logon SID, world SID]`.
//! A pre-existing world-writable directory grants Everyone (World) write.  Since
//! World is in the restricting-SID list, the sandbox can write there — an escape
//! vector.  The fix is to place a deny-write ACE for the *capability SID* (one of
//! the other restricting SIDs) on every such directory.  Because:
//!   1. Both a deny-cap ACE and the world-allow ACE must both pass the restricting
//!      SID access check; the deny wins for the cap SID even if world is allowed.
//!   2. The cap SID is a random per-workspace phantom SID that no real principal
//!      possesses, so the deny is harmless to every other user on the machine.
//!   3. No state tracking or cleanup is needed.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, PSID,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_APPEND_DATA, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, GetFileAttributesW,
    INVALID_FILE_ATTRIBUTES,
};

use super::{acl, path_norm, winutil};
use crate::WinSandboxSpec;

#[cfg(test)]
#[path = "world_writable_tests.rs"]
mod tests;

// ── Scan limits ───────────────────────────────────────────────────────────────

const AUDIT_TIME_LIMIT: Duration = Duration::from_secs(2);
const MAX_ITEMS_PER_DIR: usize = 1000;
const MAX_TOTAL_CHECKED: usize = 50_000;

/// Paths whose suffix (forward-slash, lower-case) we skip during one-level scans.
/// These are noisy Windows system directories unlikely to be used by untrusted code.
const SKIP_DIR_SUFFIXES: &[&str] = &[
    "/windows/installer",
    "/windows/registration",
    "/programdata",
    "/windows/syswow64",
    "/windows/system32",
];

// ── ACE type constants (mirrors windows-sys internal values) ──────────────────

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
/// An ACE with this flag applies only to child objects, not the object itself.
const INHERIT_ONLY_ACE: u8 = 0x08;

// ── Write mask to test for world-writable ────────────────────────────────────

const WORLD_WRITE_MASK: u32 =
    FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_EA | FILE_WRITE_ATTRIBUTES;

// ── Path attribute helpers ────────────────────────────────────────────────────

/// Return true when the path's filesystem attributes include `FILE_ATTRIBUTE_REPARSE_POINT`
/// (junction, symlink, mount-point, etc.).  Returns false on error.
fn is_reparse_point(path: &Path) -> bool {
    let wide = winutil::to_wide_path(path);
    let attrs = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attrs == INVALID_FILE_ATTRIBUTES {
        return false;
    }
    (attrs & FILE_ATTRIBUTE_REPARSE_POINT) != 0
}

/// Return true when the path's attributes include `FILE_ATTRIBUTE_DIRECTORY`.
/// Returns false on error.
fn is_directory_attr(path: &Path) -> bool {
    let wide = winutil::to_wide_path(path);
    let attrs = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attrs == INVALID_FILE_ATTRIBUTES {
        return false;
    }
    (attrs & FILE_ATTRIBUTE_DIRECTORY) != 0
}

// ── Core ACL check ────────────────────────────────────────────────────────────

/// Read the DACL of `path` and return `true` if the World (Everyone) SID is
/// explicitly granted any write-class right.
///
/// Unreadable ACLs (access denied, path vanished, etc.) are treated as
/// **not** world-writable: a warning is logged and `false` is returned.
pub(crate) fn world_has_write_access(path: &Path) -> crate::Result<bool> {
    // Build the World SID into a stack buffer.
    let mut world_buf = winutil::SidBuf::zeroed();
    world_buf.init_world_sid()?;
    let psid_world: PSID = world_buf.as_psid();

    let path_wide = winutil::to_wide_path(path);
    let mut p_sd = std::ptr::null_mut();
    let mut p_dacl: *mut ACL = std::ptr::null_mut();

    let code = unsafe {
        GetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(), // owner SID — not needed
            std::ptr::null_mut(), // group SID — not needed
            &mut p_dacl,
            std::ptr::null_mut(), // SACL — not needed
            &mut p_sd,
        )
    };

    if code != ERROR_SUCCESS {
        // Can't read ACL — treat as not world-writable (safe-fail).
        tracing::warn!(
            path = %path.display(),
            code,
            "world_writable: GetNamedSecurityInfoW failed; treating as not world-writable"
        );
        return Ok(false);
    }

    // Guard: free the security descriptor on all exit paths.
    struct SdGuard(*mut std::ffi::c_void);
    impl Drop for SdGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0 as HLOCAL) };
            }
        }
    }
    let _sd_guard = SdGuard(p_sd);

    if p_dacl.is_null() {
        // A null DACL means no restrictions — effectively world-writable.
        return Ok(true);
    }

    let found = unsafe { dacl_has_world_write_allow(p_dacl, psid_world) };
    Ok(found)
}

/// Walk the ACE list in `p_dacl` and return `true` if an `ACCESS_ALLOWED` ACE
/// for `psid_world` grants any bit from `WORLD_WRITE_MASK`.
///
/// Skips inherit-only ACEs (they don't apply to the object itself).
///
/// # Safety
/// `p_dacl` must be a valid, non-null pointer to an `ACL` structure.
/// `psid_world` must be a valid pointer to the World SID.
unsafe fn dacl_has_world_write_allow(p_dacl: *mut ACL, psid_world: PSID) -> bool {
    let mut info: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetAclInformation(
            p_dacl as *const ACL,
            &mut info as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == 0 {
        return false;
    }

    for i in 0..info.AceCount {
        let mut p_ace: *mut std::ffi::c_void = std::ptr::null_mut();
        if unsafe { GetAce(p_dacl as *const ACL, i, &mut p_ace) } == 0 {
            continue;
        }

        let hdr = unsafe { &*(p_ace as *const ACE_HEADER) };
        if hdr.AceType != ACCESS_ALLOWED_ACE_TYPE {
            continue;
        }
        // Inherit-only ACEs do not apply to the object itself.
        if (hdr.AceFlags & INHERIT_ONLY_ACE) != 0 {
            continue;
        }

        // The SID immediately follows the ACE_HEADER + the 4-byte Mask field
        // (i.e. the ACCESS_ALLOWED_ACE layout is: AceHeader, Mask, SidStart).
        let sid_ptr = (p_ace as usize
            + std::mem::size_of::<ACE_HEADER>()
            + std::mem::size_of::<u32>()) as PSID;

        if unsafe { EqualSid(sid_ptr, psid_world) } == 0 {
            continue;
        }

        // SID matches: check mask.
        let ace = unsafe { &*(p_ace as *const ACCESS_ALLOWED_ACE) };
        if (ace.Mask & WORLD_WRITE_MASK) != 0 {
            return true;
        }
    }
    false
}

// ── Candidate gathering ───────────────────────────────────────────────────────

/// Build a deduplicated list of root-level candidate directories to audit.
///
/// Order: cwd → TEMP/TMP → USERPROFILE/PUBLIC → PATH entries.
fn gather_candidates(cwd: &Path, env: &HashMap<String, String>) -> Vec<PathBuf> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<PathBuf> = Vec::new();

    let mut push = |p: PathBuf| {
        // Canonicalize for dedup, but keep the original path for scanning.
        let key = path_norm::canonical_key(&p);
        if seen.insert(key) {
            out.push(p);
        }
    };

    // 1. Working directory.
    push(cwd.to_path_buf());

    // 2. TEMP / TMP (often the most likely escape vectors).
    for var in ["TEMP", "TMP"] {
        if let Some(v) = env.get(var).cloned().or_else(|| std::env::var(var).ok()) {
            push(PathBuf::from(v));
        }
    }

    // 3. User-profile roots.
    for var in ["USERPROFILE", "PUBLIC"] {
        if let Some(v) = env.get(var).cloned().or_else(|| std::env::var(var).ok()) {
            push(PathBuf::from(v));
        }
    }

    // 4. PATH entries.
    if let Some(path_val) = env
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())
    {
        for part in std::env::split_paths(std::ffi::OsStr::new(&path_val)) {
            if !part.as_os_str().is_empty() {
                push(part);
            }
        }
    }

    out
}

fn path_key_is_equal_or_beneath(child_key: &str, parent_key: &str) -> bool {
    if child_key == parent_key {
        return true;
    }
    let prefix = if parent_key.ends_with('/') {
        parent_key.to_string()
    } else {
        format!("{parent_key}/")
    };
    child_key.starts_with(&prefix)
}

fn deny_write_ace_should_inherit(dir_key: &str, writable_root_keys: &HashSet<String>) -> bool {
    !writable_root_keys
        .iter()
        .any(|root_key| root_key != dir_key && path_key_is_equal_or_beneath(root_key, dir_key))
}

// ── Audit entry point ─────────────────────────────────────────────────────────

/// Scan candidate directories (bounded by time and count) and return those that
/// are world-writable and are NOT equal to or beneath any of `writable_root_keys`.
///
/// Only directories are returned; reparse points (junctions, symlinks) are skipped.
pub(crate) fn audit_world_writable(
    cwd: &Path,
    env: &HashMap<String, String>,
    writable_root_keys: &HashSet<String>,
) -> Vec<PathBuf> {
    let start = Instant::now();
    let mut flagged: Vec<PathBuf> = Vec::new();
    let mut seen_flagged: HashSet<String> = HashSet::new();
    let mut checked: usize = 0;

    // Returns true if the path is under (or equal to) one of the writable roots.
    let is_under_writable_root = |p: &Path| -> bool {
        let key = path_norm::canonical_key(p);
        writable_root_keys
            .iter()
            .any(|root_key| key == *root_key || key.starts_with(&format!("{}/", root_key)))
    };

    let check_and_flag =
        |p: &Path, flagged: &mut Vec<PathBuf>, seen_flagged: &mut HashSet<String>| {
            if is_reparse_point(p) || !is_directory_attr(p) {
                return;
            }
            if is_under_writable_root(p) {
                return;
            }
            match world_has_write_access(p) {
                Ok(true) => {
                    let key = path_norm::canonical_key(p);
                    if seen_flagged.insert(key) {
                        flagged.push(p.to_path_buf());
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(
                        path = %p.display(),
                        err = %e,
                        "world_writable: error checking ACL; skipping"
                    );
                }
            }
        };

    let over_limit = |start: &Instant, checked: usize| -> bool {
        start.elapsed() > AUDIT_TIME_LIMIT || checked > MAX_TOTAL_CHECKED
    };

    // Fast path: CWD immediate children first (workspace escape issues caught early).
    if !over_limit(&start, checked)
        && let Ok(read_dir) = std::fs::read_dir(cwd)
    {
        for entry in read_dir.flatten().take(MAX_ITEMS_PER_DIR) {
            if over_limit(&start, checked) {
                break;
            }
            let p = entry.path();
            // Use file_type() — it does NOT follow symlinks on Windows, so we
            // detect symlinks without traversing them.
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() || !ft.is_dir() {
                continue;
            }
            checked += 1;
            check_and_flag(&p, &mut flagged, &mut seen_flagged);
        }
    }

    // Broader candidate sweep: roots + one level of children.
    let candidates = gather_candidates(cwd, env);
    'outer: for root in &candidates {
        if over_limit(&start, checked) {
            break;
        }

        // Check the root dir itself.
        if !is_reparse_point(root) {
            checked += 1;
            if over_limit(&start, checked) {
                break;
            }
            check_and_flag(root, &mut flagged, &mut seen_flagged);
        }

        // One level of children.
        let Ok(read_dir) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in read_dir.flatten().take(MAX_ITEMS_PER_DIR) {
            if over_limit(&start, checked) {
                break 'outer;
            }
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue; // skip reparse points
            }
            if !ft.is_dir() {
                continue;
            }
            let p = entry.path();
            // Skip known-noisy system paths.
            let norm = p.to_string_lossy().to_ascii_lowercase().replace('\\', "/");
            if SKIP_DIR_SUFFIXES.iter().any(|s| norm.ends_with(s)) {
                continue;
            }
            checked += 1;
            check_and_flag(&p, &mut flagged, &mut seen_flagged);
        }
    }

    let elapsed_ms = start.elapsed().as_millis();
    if flagged.is_empty() {
        tracing::debug!(
            checked,
            elapsed_ms,
            "world_writable audit: no escape vectors found"
        );
    } else {
        tracing::warn!(
            checked,
            elapsed_ms,
            count = flagged.len(),
            "world_writable audit: found world-writable escape dirs; applying cap-SID deny ACEs"
        );
    }
    flagged
}

// ── Public harness entry point ────────────────────────────────────────────────

/// Run the world-writable audit and add a deny-write ACE for `deny_cap_sid` on
/// every escape directory that is not under a writable root.
///
/// This is best-effort: failures are logged with `tracing::warn!` but never
/// propagate (a scan failure must not prevent a spawn).
pub(crate) fn apply_world_writable_denies(
    spec: &WinSandboxSpec,
    cwd: &Path,
    env: &HashMap<String, String>,
    deny_cap_sid: &str,
) {
    // Build the set of writable-root canonical keys for exclusion.
    let writable_root_keys: HashSet<String> = spec
        .writable_roots
        .iter()
        .map(|wr| path_norm::canonical_key(&wr.root))
        .collect();

    let flagged = audit_world_writable(cwd, env, &writable_root_keys);

    for dir in &flagged {
        let dir_key = path_norm::canonical_key(dir);
        let should_inherit = deny_write_ace_should_inherit(&dir_key, &writable_root_keys);
        let result = if should_inherit {
            acl::add_deny_write_ace(dir, deny_cap_sid)
        } else {
            acl::add_deny_write_ace_no_inherit(dir, deny_cap_sid)
        };
        match result {
            Ok(()) => {
                tracing::debug!(
                    path = %dir.display(),
                    sid = deny_cap_sid,
                    inherit = should_inherit,
                    "world_writable: applied cap-SID deny-write ACE"
                );
            }
            Err(e) => {
                tracing::warn!(
                    path = %dir.display(),
                    sid = deny_cap_sid,
                    inherit = should_inherit,
                    err = %e,
                    "world_writable: failed to apply cap-SID deny-write ACE; continuing"
                );
            }
        }
    }
}
