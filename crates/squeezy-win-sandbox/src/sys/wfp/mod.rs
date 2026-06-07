//! WFP (Windows Filtering Platform) egress-blocking for the offline sandbox user.
//!
//! `install_block_filters(account_sid)` installs 12 persistent block filters
//! scoped to the offline sandbox account SID.  `remove_filters()` removes them.
//! Both functions open a WFP engine session, wrap the work in a transaction, and
//! RAII-close the engine on exit.  The filters are persistent (survive reboots).
//!
//! These entry points are called from the already-elevated setup path; all calls
//! are `unsafe` because they cross the Win32 FFI boundary.

mod filter_specs;

use std::mem::zeroed;
use std::ptr::null;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{
    FWP_E_ALREADY_EXISTS, FWP_E_FILTER_NOT_FOUND, FWP_E_NOT_FOUND, HLOCAL, LocalFree,
};
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::{
    FWP_ACTION_BLOCK, FWP_ACTRL_MATCH_FILTER, FWP_BYTE_BLOB, FWP_CONDITION_VALUE0,
    FWP_CONDITION_VALUE0_0, FWP_EMPTY, FWP_MATCH_EQUAL, FWP_SECURITY_DESCRIPTOR_TYPE, FWP_UINT16,
    FWP_UINT8, FWP_VALUE0, FWPM_ACTION0, FWPM_ACTION0_0, FWPM_CONDITION_ALE_USER_ID,
    FWPM_CONDITION_IP_PROTOCOL, FWPM_CONDITION_IP_REMOTE_PORT, FWPM_DISPLAY_DATA0,
    FWPM_FILTER0, FWPM_FILTER0_0, FWPM_FILTER_CONDITION0, FWPM_FILTER_FLAG_PERSISTENT,
    FWPM_PROVIDER0, FWPM_PROVIDER_FLAG_PERSISTENT, FWPM_SESSION0, FWPM_SUBLAYER0,
    FWPM_SUBLAYER_FLAG_PERSISTENT, FwpmEngineClose0, FwpmEngineOpen0, FwpmFilterAdd0,
    FwpmFilterDeleteByKey0, FwpmProviderAdd0, FwpmProviderDeleteByKey0, FwpmSubLayerAdd0,
    FwpmSubLayerDeleteByKey0, FwpmTransactionAbort0, FwpmTransactionBegin0,
    FwpmTransactionCommit0,
};
use windows_sys::Win32::Security::Authorization::{
    BuildExplicitAccessWithNameW, BuildSecurityDescriptorW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
};
use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
use windows_sys::Win32::System::Rpc::RPC_C_AUTHN_WINNT;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::core::GUID;

use super::winutil::to_wide;

use filter_specs::{ConditionSpec, FILTER_SPECS, FilterSpec, PROVIDER_KEY, SUBLAYER_KEY};

const SESSION_NAME: &str = "Squeezy Sandbox WFP Session";
const PROVIDER_NAME: &str = "Squeezy Sandbox WFP";
const PROVIDER_DESC: &str = "Persistent WFP provider for Squeezy sandbox egress blocking";
const SUBLAYER_NAME: &str = "Squeezy Sandbox WFP Sublayer";
const SUBLAYER_DESC: &str = "Persistent WFP sublayer for Squeezy sandbox egress blocking";

// ── RAII engine handle ────────────────────────────────────────────────────────

struct Engine {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

impl Engine {
    fn open() -> crate::Result<Self> {
        let session_name = to_wide(SESSION_NAME);
        let mut session: FWPM_SESSION0 = unsafe { zeroed() };
        session.displayData = FWPM_DISPLAY_DATA0 {
            name: session_name.as_ptr() as *mut _,
            description: null_mut(),
        };
        // INFINITE means transaction wait never times out.
        session.txnWaitTimeoutInMSec = INFINITE;
        // Flags = 0 → persistent session (filters survive engine close).

        let mut handle: windows_sys::Win32::Foundation::HANDLE =
            unsafe { zeroed() };
        let rc = unsafe {
            FwpmEngineOpen0(
                null(),
                RPC_C_AUTHN_WINNT,
                null(),
                &session,
                &mut handle,
            )
        };
        wfp_ok(rc, "FwpmEngineOpen0")?;
        Ok(Self { handle })
    }

    fn begin_transaction(&self) -> crate::Result<Transaction<'_>> {
        let rc = unsafe { FwpmTransactionBegin0(self.handle, 0) };
        wfp_ok(rc, "FwpmTransactionBegin0")?;
        Ok(Transaction {
            engine: self,
            committed: false,
        })
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe {
            FwpmEngineClose0(self.handle);
        }
    }
}

// ── RAII transaction guard ────────────────────────────────────────────────────

struct Transaction<'a> {
    engine: &'a Engine,
    committed: bool,
}

impl Transaction<'_> {
    fn commit(&mut self) -> crate::Result<()> {
        let rc = unsafe { FwpmTransactionCommit0(self.engine.handle) };
        wfp_ok(rc, "FwpmTransactionCommit0")?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.committed {
            unsafe {
                FwpmTransactionAbort0(self.engine.handle);
            }
        }
    }
}

// ── User-match condition (ALE_USER_ID + security descriptor blob) ─────────────

/// Owns the self-relative security descriptor produced by `BuildSecurityDescriptorW`
/// and keeps the `FWP_BYTE_BLOB` pointing into it.
struct UserMatchCondition {
    sd: PSECURITY_DESCRIPTOR,
    blob: FWP_BYTE_BLOB,
}

impl UserMatchCondition {
    /// Build a security descriptor that grants `FWP_ACTRL_MATCH_FILTER` to the
    /// named account (identified by SID string like `"S-1-5-21-…"`).
    ///
    /// `BuildExplicitAccessWithNameW` accepts both username strings and SID
    /// strings — passing the SID string directly avoids an extra account-lookup
    /// round-trip and works even while the account is still being set up.
    fn for_sid(sid_str: &str) -> crate::Result<Self> {
        let name_wide = to_wide(sid_str);

        // Build an EXPLICIT_ACCESS_W that grants FWP_ACTRL_MATCH_FILTER.
        let mut access: EXPLICIT_ACCESS_W = unsafe { zeroed() };
        unsafe {
            BuildExplicitAccessWithNameW(
                &mut access,
                name_wide.as_ptr() as *mut _,
                FWP_ACTRL_MATCH_FILTER,
                GRANT_ACCESS,
                0, // no inheritance
            );
        }

        // BuildSecurityDescriptorW allocates a self-relative SD via LocalAlloc.
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let mut sd_size: u32 = 0;
        let rc = unsafe {
            BuildSecurityDescriptorW(
                null_mut(), // owner: keep default
                null_mut(), // group: keep default
                1,          // count of EXPLICIT_ACCESS entries
                &access,
                0,           // no deny ACEs
                null_mut(),  // no deny-ACE array
                null_mut(),  // no existing descriptor to merge
                &mut sd_size,
                &mut sd,
            )
        };
        wfp_ok(rc, "BuildSecurityDescriptorW")?;

        Ok(Self {
            sd,
            blob: FWP_BYTE_BLOB {
                size: sd_size,
                data: sd as *mut u8,
            },
        })
    }
}

impl Drop for UserMatchCondition {
    fn drop(&mut self) {
        if !self.sd.is_null() {
            unsafe {
                LocalFree(self.sd as HLOCAL);
            }
        }
    }
}

// ── Helper: build FWPM_FILTER_CONDITION0 vector ───────────────────────────────

fn build_conditions(
    specs: &[ConditionSpec],
    user: &UserMatchCondition,
) -> Vec<FWPM_FILTER_CONDITION0> {
    specs
        .iter()
        .map(|s| match s {
            ConditionSpec::User => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_ALE_USER_ID,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_SECURITY_DESCRIPTOR_TYPE,
                    Anonymous: FWP_CONDITION_VALUE0_0 {
                        // The blob is alive for the duration of FwpmFilterAdd0.
                        sd: &user.blob as *const _ as *mut _,
                    },
                },
            },
            ConditionSpec::Protocol(proto) => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_IP_PROTOCOL,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_UINT8,
                    Anonymous: FWP_CONDITION_VALUE0_0 { uint8: *proto },
                },
            },
            ConditionSpec::RemotePort(port) => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_IP_REMOTE_PORT,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_UINT16,
                    Anonymous: FWP_CONDITION_VALUE0_0 { uint16: *port },
                },
            },
        })
        .collect()
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn empty_blob() -> FWP_BYTE_BLOB {
    FWP_BYTE_BLOB {
        size: 0,
        data: null_mut(),
    }
}

fn empty_value() -> FWP_VALUE0 {
    FWP_VALUE0 {
        r#type: FWP_EMPTY,
        Anonymous: unsafe { zeroed() },
    }
}

fn zero_guid() -> GUID {
    GUID {
        data1: 0,
        data2: 0,
        data3: 0,
        data4: [0u8; 8],
    }
}

/// Returns `Ok(())` when `rc == 0` (ERROR_SUCCESS).  Errors not in the
/// `allowed` slice surface as a `WinSandboxError::Win32`.
fn wfp_ok_or(rc: u32, op: &str, allowed: &[u32]) -> crate::Result<()> {
    if rc == 0 || allowed.contains(&rc) {
        Ok(())
    } else {
        Err(crate::WinSandboxError::win32(format!(
            "{op} failed: 0x{rc:08X}"
        )))
    }
}

fn wfp_ok(rc: u32, op: &str) -> crate::Result<()> {
    wfp_ok_or(rc, op, &[])
}

// ── Ensure provider / sublayer ────────────────────────────────────────────────

fn ensure_provider(engine: windows_sys::Win32::Foundation::HANDLE) -> crate::Result<()> {
    let name = to_wide(PROVIDER_NAME);
    let desc = to_wide(PROVIDER_DESC);
    let provider = FWPM_PROVIDER0 {
        providerKey: PROVIDER_KEY,
        displayData: FWPM_DISPLAY_DATA0 {
            name: name.as_ptr() as *mut _,
            description: desc.as_ptr() as *mut _,
        },
        flags: FWPM_PROVIDER_FLAG_PERSISTENT,
        providerData: empty_blob(),
        serviceName: null_mut(),
    };
    let rc = unsafe { FwpmProviderAdd0(engine, &provider, null_mut()) };
    wfp_ok_or(rc, "FwpmProviderAdd0", &[FWP_E_ALREADY_EXISTS as u32])
}

fn ensure_sublayer(engine: windows_sys::Win32::Foundation::HANDLE) -> crate::Result<()> {
    let name = to_wide(SUBLAYER_NAME);
    let desc = to_wide(SUBLAYER_DESC);
    // providerKey must be a mutable pointer to a GUID on the stack.
    let provider_key = PROVIDER_KEY;
    let sublayer = FWPM_SUBLAYER0 {
        subLayerKey: SUBLAYER_KEY,
        displayData: FWPM_DISPLAY_DATA0 {
            name: name.as_ptr() as *mut _,
            description: desc.as_ptr() as *mut _,
        },
        flags: FWPM_SUBLAYER_FLAG_PERSISTENT,
        providerKey: &provider_key as *const GUID as *mut GUID,
        providerData: empty_blob(),
        weight: 0x8000, // mid-range priority
    };
    let rc = unsafe { FwpmSubLayerAdd0(engine, &sublayer, null_mut()) };
    wfp_ok_or(rc, "FwpmSubLayerAdd0", &[FWP_E_ALREADY_EXISTS as u32])
}

// ── Install one filter ────────────────────────────────────────────────────────

fn add_filter(
    engine: windows_sys::Win32::Foundation::HANDLE,
    spec: &FilterSpec,
    user: &UserMatchCondition,
) -> crate::Result<()> {
    let name = to_wide(spec.name);
    let mut conditions = build_conditions(spec.conditions, user);
    let provider_key = PROVIDER_KEY;
    let filter = FWPM_FILTER0 {
        filterKey: spec.key,
        displayData: FWPM_DISPLAY_DATA0 {
            name: name.as_ptr() as *mut _,
            description: null_mut(),
        },
        flags: FWPM_FILTER_FLAG_PERSISTENT,
        providerKey: &provider_key as *const GUID as *mut GUID,
        providerData: empty_blob(),
        layerKey: spec.layer_key,
        subLayerKey: SUBLAYER_KEY,
        weight: empty_value(),
        numFilterConditions: conditions.len() as u32,
        filterCondition: conditions.as_mut_ptr(),
        action: FWPM_ACTION0 {
            r#type: FWP_ACTION_BLOCK,
            Anonymous: FWPM_ACTION0_0 {
                filterType: zero_guid(),
            },
        },
        Anonymous: FWPM_FILTER0_0 { rawContext: 0 },
        reserved: null_mut(),
        filterId: 0,
        effectiveWeight: empty_value(),
    };
    let mut filter_id: u64 = 0;
    let rc = unsafe { FwpmFilterAdd0(engine, &filter, null_mut(), &mut filter_id) };
    wfp_ok(rc, &format!("FwpmFilterAdd0({})", spec.name))
}

// ── Delete one filter (idempotent) ────────────────────────────────────────────

fn delete_filter_if_present(
    engine: windows_sys::Win32::Foundation::HANDLE,
    key: &GUID,
) -> crate::Result<()> {
    let rc = unsafe { FwpmFilterDeleteByKey0(engine, key) };
    wfp_ok_or(
        rc,
        "FwpmFilterDeleteByKey0",
        &[FWP_E_FILTER_NOT_FOUND as u32, FWP_E_NOT_FOUND as u32],
    )
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Install all 12 persistent WFP block filters scoped to `account_sid`.
///
/// Opens a non-dynamic engine session (filters survive the session), wraps the
/// work in a WFP transaction, and RAII-aborts on error.  Idempotent: each
/// filter key is deleted before re-adding so re-running setup does not produce
/// duplicate entries.
///
/// Returns the count of filters successfully added (should be 12 on success).
pub(crate) fn install_block_filters(account_sid: &str) -> crate::Result<usize> {
    let engine = Engine::open()?;
    let mut tx = engine.begin_transaction()?;

    ensure_provider(engine.handle)?;
    ensure_sublayer(engine.handle)?;

    let user = UserMatchCondition::for_sid(account_sid)?;

    let mut installed = 0usize;
    for spec in FILTER_SPECS {
        delete_filter_if_present(engine.handle, &spec.key)?;
        add_filter(engine.handle, spec, &user)?;
        installed += 1;
    }

    tx.commit()?;
    Ok(installed)
}

/// Remove all 12 persistent WFP block filters, the sublayer, and the provider.
///
/// Counts only the filters successfully deleted (not the sublayer/provider).
/// Idempotent: not-found errors are treated as success.
///
/// Returns the count of filters removed.
pub(crate) fn remove_filters() -> crate::Result<usize> {
    let engine = Engine::open()?;
    let mut tx = engine.begin_transaction()?;

    let mut removed = 0usize;
    for spec in FILTER_SPECS {
        let rc = unsafe { FwpmFilterDeleteByKey0(engine.handle, &spec.key) };
        if rc == 0 {
            removed += 1;
        }
        // Ignore FWP_E_FILTER_NOT_FOUND / FWP_E_NOT_FOUND — already gone.
    }

    // Best-effort: remove sublayer then provider (ignore not-found).
    let rc = unsafe { FwpmSubLayerDeleteByKey0(engine.handle, &SUBLAYER_KEY) };
    let _ = wfp_ok_or(
        rc,
        "FwpmSubLayerDeleteByKey0",
        &[FWP_E_NOT_FOUND as u32],
    );

    let rc = unsafe { FwpmProviderDeleteByKey0(engine.handle, &PROVIDER_KEY) };
    let _ = wfp_ok_or(
        rc,
        "FwpmProviderDeleteByKey0",
        &[FWP_E_NOT_FOUND as u32],
    );

    tx.commit()?;
    Ok(removed)
}
