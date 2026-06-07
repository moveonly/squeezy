//! Elevated helper binary for `squeezy-win-sandbox`.
//!
//! Launched by the orchestrator via `ShellExecuteExW` with verb `"runas"` when
//! the calling process is not already elevated.  On success exits 0; on failure
//! writes `<state_dir>/setup_error.json` and exits non-zero.
//!
//! Usage: `squeezy-sandbox-setup <base64-json-payload>`
//!
//! The payload is a JSON-serialised `ElevationPayload` (see `setup.rs`), then
//! base64-encoded.

#[cfg(windows)]
fn main() {
    use std::process;

    match run() {
        Ok(()) => process::exit(0),
        Err(msg) => {
            eprintln!("squeezy-sandbox-setup error: {msg}");
            process::exit(1);
        }
    }
}

#[cfg(windows)]
fn run() -> Result<(), String> {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use serde::Deserialize;
    use squeezy_win_sandbox::{
        WinNetwork, WinSandboxError, WinSandboxSpec, WinTokenMode, WinWritableRoot,
    };
    use squeezy_win_sandbox::{SETUP_VERSION, run_setup_privileged};
    use std::path::PathBuf;

    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        return Err("expected exactly one argument (base64 payload)".to_string());
    }
    let payload_b64 = &args[1];

    let payload_json = BASE64
        .decode(payload_b64.as_bytes())
        .map_err(|e| format!("base64 decode: {e}"))?;

    #[derive(Deserialize)]
    struct SerWritableRoot {
        root: PathBuf,
        #[serde(default)]
        read_only_subpaths: Vec<PathBuf>,
    }

    #[derive(Deserialize)]
    struct ElevationPayload {
        version: u32,
        state_dir: PathBuf,
        #[serde(default)]
        writable_roots: Vec<SerWritableRoot>,
        #[serde(default)]
        read_roots: Vec<PathBuf>,
        #[serde(default)]
        deny_read_paths: Vec<PathBuf>,
        #[serde(default)]
        protected_metadata_names: Vec<String>,
        #[serde(default)]
        sensitive_path_patterns: Vec<String>,
    }

    let payload: ElevationPayload = serde_json::from_slice(&payload_json)
        .map_err(|e| format!("JSON parse: {e}"))?;

    if payload.version != SETUP_VERSION {
        return Err(format!(
            "payload version {} does not match expected {SETUP_VERSION}",
            payload.version
        ));
    }

    let writable_roots = payload
        .writable_roots
        .into_iter()
        .map(|w| WinWritableRoot {
            root: w.root,
            read_only_subpaths: w.read_only_subpaths,
        })
        .collect::<Vec<_>>();

    let spec = WinSandboxSpec {
        token_mode: WinTokenMode::WritableRootsCapability,
        writable_roots,
        read_roots: payload.read_roots,
        deny_read_paths: payload.deny_read_paths,
        protected_metadata_names: payload.protected_metadata_names,
        sensitive_path_patterns: payload.sensitive_path_patterns,
        network: WinNetwork::Unenforced,
        state_dir: payload.state_dir,
    };

    // run_setup_privileged writes setup_error.json on failure internally.
    run_setup_privileged(&spec).map_err(|e: WinSandboxError| e.to_string())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("squeezy-sandbox-setup is windows-only");
    std::process::exit(1);
}
