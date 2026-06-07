//! Capability-SID minting and persistence.
//!
//! Capability SIDs are synthetic domain SIDs (`S-1-5-21-a-b-c-d`) that gate
//! write access on specific workspace roots.  They are generated once,
//! persisted as JSON, and reloaded on every subsequent spawn.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::path_norm;
use crate::WinSandboxError;

// ── SID minting ───────────────────────────────────────────────────────────────

/// Mint a random domain-style capability SID string `S-1-5-21-{a}-{b}-{c}-{d}`.
fn mint_cap_sid() -> String {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect("getrandom::fill failed");
    let a = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    let b = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    let c = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    let d = u32::from_le_bytes(buf[12..16].try_into().unwrap());
    format!("S-1-5-21-{a}-{b}-{c}-{d}")
}

// ── Persistent store ──────────────────────────────────────────────────────────

/// JSON schema for `<state_dir>/win-sandbox/cap_sids.json`.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct CapSids {
    readonly: String,
    #[serde(default)]
    writable_root_by_key: BTreeMap<String, String>,
}

fn cap_sids_path(state_dir: &Path) -> PathBuf {
    state_dir.join("win-sandbox").join("cap_sids.json")
}

fn load_cap_sids(state_dir: &Path) -> crate::Result<Option<CapSids>> {
    let path = cap_sids_path(state_dir);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|_| WinSandboxError::Io(std::io::Error::other("failed to read cap_sids.json")))?;
    let caps: CapSids = serde_json::from_str(&text)
        .map_err(|e| WinSandboxError::win32(format!("cap_sids.json parse error: {e}")))?;
    Ok(Some(caps))
}

fn save_cap_sids(state_dir: &Path, caps: &CapSids) -> crate::Result<()> {
    let path = cap_sids_path(state_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| {
            WinSandboxError::Io(std::io::Error::other("failed to create cap_sids dir"))
        })?;
    }
    let json = serde_json::to_string(caps).expect("CapSids serialization failed");
    std::fs::write(&path, json)
        .map_err(|_| WinSandboxError::Io(std::io::Error::other("failed to write cap_sids.json")))?;
    Ok(())
}

fn load_or_create(state_dir: &Path) -> crate::Result<CapSids> {
    if let Some(caps) = load_cap_sids(state_dir)? {
        return Ok(caps);
    }
    let caps = CapSids {
        readonly: mint_cap_sid(),
        writable_root_by_key: BTreeMap::new(),
    };
    save_cap_sids(state_dir, &caps)?;
    Ok(caps)
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Return (loading or minting) the read-only capability SID.
pub(crate) fn readonly_cap_sid(state_dir: &Path) -> crate::Result<String> {
    Ok(load_or_create(state_dir)?.readonly)
}

/// Return (loading or minting) the per-root write capability SID, keyed by
/// the canonical path string of `root`.
pub(crate) fn writable_root_cap_sid(state_dir: &Path, root: &Path) -> crate::Result<String> {
    let key = path_norm::canonical_key(root);
    let mut caps = load_or_create(state_dir)?;
    if let Some(sid) = caps.writable_root_by_key.get(&key) {
        return Ok(sid.clone());
    }
    let sid = mint_cap_sid();
    caps.writable_root_by_key.insert(key, sid.clone());
    save_cap_sids(state_dir, &caps)?;
    Ok(sid)
}
