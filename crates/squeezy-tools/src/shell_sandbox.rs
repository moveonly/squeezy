#[cfg(target_os = "macos")]
use std::process::Stdio;
use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    sync::{
        Mutex as StdMutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::env;

#[cfg(target_os = "linux")]
use std::{fs::OpenOptions, io::Write};

use serde_json::{Value, json};
#[cfg(target_os = "macos")]
use squeezy_core::sensitive_pattern_base;
use squeezy_core::{ShellSandboxConfig, ShellSandboxMode, ShellSandboxNetworkPolicy};
use tokio::process::Command;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::shell_exit_signal;
use crate::shell_program::ShellProgram;
use crate::{ShellPermissionAnalysis, ShellRunOutcome};

pub(crate) const SHELL_SANDBOX_BACKEND_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub(crate) enum ShellSandboxBackendStatus {
    Available,
    Unavailable(String),
}

#[derive(Debug, Default)]
pub(crate) struct ShellSandboxHealth {
    backends: StdMutex<HashMap<&'static str, ShellSandboxBackendStatus>>,
    /// Lifetime count of best_effort fallbacks observed by this registry,
    /// across all backends. The agent layer pivots on this to keep the
    /// `approval.best_effort.fallback` telemetry counter in step with the
    /// runtime — every increment fires one telemetry event.
    best_effort_fallback_count: AtomicU64,
    /// One-shot latch so the user-visible TUI warning fires at most once
    /// per session, even when several shell calls in a row land on the
    /// same degraded backend. The telemetry counter above keeps ticking.
    best_effort_warning_emitted: AtomicBool,
    /// macOS SBPL profile cache: computed once per session per `allow_network`
    /// value (deny=index 0, allow=index 1). The profile string is
    /// deterministic for a fixed `(root, config)` pair. **Invariant**: this
    /// `ShellSandboxHealth` must not be shared across different roots or
    /// across config reloads — the profile depends on `config.read_roots`,
    /// `config.write_roots`, `config.protected_metadata_names`,
    /// `config.sensitive_path_patterns`, and
    /// `config.macos_socket_domain_allowlist`. The `ToolRegistry` satisfies
    /// this invariant by constructing a fresh health instance alongside every
    /// new `(root, shell_sandbox)` pair.
    #[cfg(target_os = "macos")]
    pub(crate) sbpl_profile_cache: [OnceLock<String>; 2],
    /// Allowlisted environment map cache. `apply_shell_environment_policy`
    /// scans all env vars and filters by the allowlist on every shell call;
    /// since the agent process environment and the allowlist are both stable
    /// per session, we compute this once and clone it per call instead.
    pub(crate) preserved_env_cache:
        OnceLock<std::collections::BTreeMap<String, std::ffi::OsString>>,
    /// One-shot latch for the Windows-specific degradation banner. Windows
    /// shell runs always use `windows-job-object` with no FS/network
    /// isolation; the TUI should warn once per session rather than on
    /// every call.
    windows_degraded_warned: AtomicBool,
}

/// Outcome of `ShellSandboxHealth::record_best_effort_fallback`. The agent
/// reads `fallback_count` to drive the telemetry counter and `first_in_session`
/// to decide whether to surface the user-facing TUI warning. Returning both
/// in one struct keeps the (count, latch) update atomic — callers never see
/// the counter advance without also learning whether they're the warner.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ShellSandboxFallbackRecord {
    pub(crate) fallback_count: u64,
    pub(crate) first_in_session: bool,
}

impl ShellSandboxHealth {
    pub(crate) fn status(&self, backend: &'static str) -> Option<ShellSandboxBackendStatus> {
        self.backends
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(backend)
            .cloned()
    }

    pub(crate) fn mark_available(&self, backend: &'static str) {
        if backend == "none" {
            return;
        }
        self.backends
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(backend, ShellSandboxBackendStatus::Available);
    }

    pub(crate) fn mark_unavailable(&self, backend: &'static str, reason: impl Into<String>) {
        if backend == "none" {
            return;
        }
        self.backends
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(
                backend,
                ShellSandboxBackendStatus::Unavailable(reason.into()),
            );
    }

    /// Bump the cumulative best_effort fallback counter and report whether
    /// this is the first occurrence in the session so the caller can
    /// publish a one-shot TUI warning. The counter keeps incrementing on
    /// every call so telemetry dashboards see each silent degradation.
    pub(crate) fn record_best_effort_fallback(&self) -> ShellSandboxFallbackRecord {
        // `fetch_add` on a `Relaxed` ordering is fine here: this is a
        // monotonic counter consumed by the same registry that produced
        // it, so we don't need to fence between the count update and the
        // latch flip below.
        let prev = self
            .best_effort_fallback_count
            .fetch_add(1, Ordering::Relaxed);
        let first_in_session = !self
            .best_effort_warning_emitted
            .swap(true, Ordering::Relaxed);
        ShellSandboxFallbackRecord {
            fallback_count: prev.saturating_add(1),
            first_in_session,
        }
    }

    #[cfg(test)]
    pub(crate) fn best_effort_fallback_count(&self) -> u64 {
        self.best_effort_fallback_count.load(Ordering::Relaxed)
    }

    /// Record a Windows-specific shell degradation event and return whether
    /// this is the first occurrence in the session. The caller embeds the
    /// returned flag into the result so the agent can emit a once-per-session
    /// TUI warning without requiring a side channel.
    pub(crate) fn record_windows_degraded(&self) -> bool {
        !self.windows_degraded_warned.swap(true, Ordering::Relaxed)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ShellSandboxPlan {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) backend: &'static str,
    pub(crate) mode: &'static str,
    pub(crate) network: &'static str,
    pub(crate) filesystem: &'static str,
    pub(crate) required: bool,
    pub(crate) configured_read_roots: Vec<PathBuf>,
    pub(crate) configured_write_roots: Vec<PathBuf>,
    #[allow(dead_code)]
    pub(crate) filesystem_read_roots: Vec<PathBuf>,
    #[allow(dead_code)]
    pub(crate) filesystem_write_roots: Vec<PathBuf>,
    pub(crate) fallback_reason: Option<String>,
    /// When this plan represents a best_effort fallback after a sandbox
    /// failure, carries the originating backend plus the session counter
    /// snapshot. Lets the audit row and the JSON surfaced to the agent
    /// (which then drives telemetry + a one-shot TUI warning) describe
    /// the degradation without a side channel.
    pub(crate) best_effort_fallback: Option<BestEffortFallback>,
    /// Short display name of the shell that will execute the command.
    /// Populated for Windows plans (`"pwsh"`, `"powershell"`, `"cmd.exe"`,
    /// `"gitbash"`, or a custom basename). `None` on Unix (where the shell is
    /// always `sh` or a sandbox wrapper and is implicit from the backend).
    pub(crate) selected_shell: Option<String>,
}

/// Side-table that accompanies a best_effort fallback plan. The agent
/// reads these fields to (a) tick the `approval.best_effort.fallback`
/// telemetry counter and (b) decide whether to publish the once-per-session
/// TUI banner. `fallback_reason` is surfaced in the TUI warning so users
/// see whether the degradation came from a probe timeout, a spawn/pre-exec
/// error, a signal-killed probe child, or a cached-unavailable backend.
#[derive(Debug, Clone)]
pub(crate) struct BestEffortFallback {
    pub(crate) backend: &'static str,
    pub(crate) fallback_count: u64,
    pub(crate) first_in_session: bool,
    pub(crate) fallback_reason: Option<String>,
}

impl ShellSandboxPlan {
    pub(crate) fn direct(
        command: &str,
        mode: ShellSandboxMode,
        config: &ShellSandboxConfig,
    ) -> Self {
        Self::direct_with_fallback(command, mode, config, None)
    }

    pub(crate) fn direct_with_fallback(
        command: &str,
        mode: ShellSandboxMode,
        config: &ShellSandboxConfig,
        fallback_reason: Option<String>,
    ) -> Self {
        Self::direct_with_fallback_record(command, mode, config, fallback_reason, None)
    }

    pub(crate) fn direct_with_fallback_record(
        command: &str,
        mode: ShellSandboxMode,
        config: &ShellSandboxConfig,
        fallback_reason: Option<String>,
        best_effort: Option<(&'static str, ShellSandboxFallbackRecord)>,
    ) -> Self {
        let shell = ShellProgram::for_command(command);
        let selected_shell = windows_shell_display_name(&shell);
        Self {
            program: shell.program,
            args: shell.args,
            backend: "none",
            mode: mode.as_str(),
            network: "not_enforced",
            filesystem: "not_enforced",
            required: false,
            configured_read_roots: config.read_roots.clone(),
            configured_write_roots: config.write_roots.clone(),
            filesystem_read_roots: Vec::new(),
            filesystem_write_roots: Vec::new(),
            best_effort_fallback: best_effort.map(|(backend, record)| BestEffortFallback {
                backend,
                fallback_count: record.fallback_count,
                first_in_session: record.first_in_session,
                fallback_reason: fallback_reason.clone(),
            }),
            fallback_reason,
            selected_shell,
        }
    }

    pub(crate) fn external(command: &str, config: &ShellSandboxConfig) -> Self {
        let shell = ShellProgram::for_command(command);
        let selected_shell = windows_shell_display_name(&shell);
        Self {
            program: shell.program,
            args: shell.args,
            backend: "external",
            mode: ShellSandboxMode::External.as_str(),
            network: "external",
            filesystem: "external",
            required: false,
            configured_read_roots: config.read_roots.clone(),
            configured_write_roots: config.write_roots.clone(),
            filesystem_read_roots: Vec::new(),
            filesystem_write_roots: Vec::new(),
            fallback_reason: None,
            best_effort_fallback: None,
            selected_shell,
        }
    }

    pub(crate) fn metadata(&self) -> Value {
        #[cfg(target_os = "linux")]
        let shell = if self.backend == "linux-direct-syscalls"
            || (self.backend == "none" && self.program.starts_with('/'))
        {
            Some(self.program.as_str())
        } else {
            None
        };
        #[cfg(not(target_os = "linux"))]
        let shell = if self.backend == "linux-direct-syscalls" {
            Some(self.program.as_str())
        } else {
            None
        };
        // Whether the `squeezy ask` AF_UNIX callback socket is suppressed for
        // this backend. True for linux-direct-syscalls (seccomp denies AF_UNIX)
        // and the Windows sandbox tiers (no Unix socket transport).
        let ask_socket_suppressed = !self.exports_ask_socket();
        // Whether Landlock filesystem enforcement is active. On linux-direct-syscalls
        // this maps to filesystem == "enforced"; other backends do not use Landlock.
        let landlock_active =
            self.backend == "linux-direct-syscalls" && self.filesystem == "enforced";
        let mut payload = json!({
            "backend": self.backend,
            "mode": self.mode,
            "network": self.network,
            "filesystem": self.filesystem,
            "required": self.required,
            "read_roots": path_list_json(&self.configured_read_roots),
            "write_roots": path_list_json(&self.configured_write_roots),
            "fallback_reason": self.fallback_reason,
            // Include the effective Linux shell even when best_effort has
            // degraded to a direct spawn that still honors linux_shell.
            "shell": shell,
            "ask_socket_suppressed": ask_socket_suppressed,
            "landlock_active": landlock_active,
        });
        if let Some(shell) = &self.selected_shell
            && let Some(object) = payload.as_object_mut()
        {
            object.insert("selected_shell".to_string(), serde_json::json!(shell));
        }
        if let Some(record) = &self.best_effort_fallback
            && let Some(object) = payload.as_object_mut()
        {
            object.insert(
                "best_effort_fallback".to_string(),
                best_effort_fallback_json(record.clone()),
            );
        }
        payload
    }

    /// Build the audit metadata row at the fallback EMISSION site, where
    /// the plan still references the failing backend (so `backend`,
    /// `filesystem`, etc. describe the attempt that was abandoned) but
    /// the caller already has the counter snapshot in hand. `reason` is the
    /// human-readable degradation reason forwarded into `fallback_reason` so
    /// the TUI warning can explain the root cause to the user.
    pub(crate) fn metadata_with_best_effort_fallback(
        &self,
        degraded_backend: &'static str,
        record: &ShellSandboxFallbackRecord,
        reason: Option<&str>,
    ) -> Value {
        let mut payload = self.metadata();
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "best_effort_fallback".to_string(),
                best_effort_fallback_json(BestEffortFallback {
                    backend: degraded_backend,
                    fallback_count: record.fallback_count,
                    first_in_session: record.first_in_session,
                    fallback_reason: reason.map(str::to_owned),
                }),
            );
        }
        payload
    }

    /// Whether the in-flight `squeezy ask` approval socket can be exported
    /// into the shell child under this plan's backend.
    ///
    /// The `linux-direct-syscalls` backend installs a seccomp filter that
    /// denies `socket(AF_UNIX, …)` (see [`configure_linux_shell_sandbox`]),
    /// so a child that runs `squeezy ask` could never `UnixStream::connect`
    /// to the socket — the connect is `EPERM`-ed before any handshake.
    /// Advertising `SQUEEZY_ASK_SOCKET` to such a child would promise a
    /// capability that is guaranteed to fail with a confusing errno; the
    /// child instead gets the clear "not set" path from `squeezy ask`.
    pub(crate) fn exports_ask_socket(&self) -> bool {
        // The Windows sandbox backends spawn via raw Win32 with a scrubbed
        // environment and have no AF_UNIX `squeezy ask` transport wired up yet,
        // so exporting the socket would advertise an unusable capability.
        !matches!(
            self.backend,
            "linux-direct-syscalls" | "windows-restricted-token" | "windows-elevated"
        )
    }

    /// When `exports_ask_socket` is false, returns a human-readable reason
    /// explaining why nested `squeezy ask` approvals are unavailable under
    /// this sandbox backend. Returns `None` when ask is available.
    ///
    /// The reason is differentiated by backend to explain the actual
    /// mechanism (seccomp filter vs. missing Win32 transport) rather than
    /// just naming the backend.
    pub(crate) fn nested_ask_disabled_reason(&self) -> Option<String> {
        if self.exports_ask_socket() {
            return None;
        }
        let reason = match self.backend {
            "linux-direct-syscalls" => {
                "nested ask disabled: seccomp filter denies AF_UNIX socket(2) in this sandbox"
                    .to_string()
            }
            "windows-restricted-token" | "windows-elevated" => {
                "nested ask disabled: Win32 sandbox spawn has no AF_UNIX ask transport".to_string()
            }
            other => format!("nested ask disabled by {} sandbox", other),
        };
        Some(reason)
    }
}

fn best_effort_fallback_json(record: BestEffortFallback) -> Value {
    json!({
        "backend": record.backend,
        "fallback_count": record.fallback_count,
        "first_in_session": record.first_in_session,
        "fallback_reason": record.fallback_reason,
    })
}

pub(crate) fn path_list_json(paths: &[PathBuf]) -> Value {
    Value::Array(
        paths
            .iter()
            .map(|path| Value::String(path.display().to_string()))
            .collect(),
    )
}

pub(crate) fn shell_sandbox_status_metadata(config: &ShellSandboxConfig, status: &str) -> Value {
    json!({
        "backend": "none",
        "mode": config.mode.as_str(),
        "network": "not_enforced",
        "filesystem": "not_enforced",
        "required": false,
        "status": status,
        "read_roots": path_list_json(&config.read_roots),
        "write_roots": path_list_json(&config.write_roots),
    })
}

pub(crate) fn macos_sandbox_exec_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        #[cfg(target_os = "macos")]
        {
            Path::new("/usr/bin/sandbox-exec").exists()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    })
}

/// Per-platform shell-sandbox posture for `squeezy doctor`, reflecting the
/// ACTUAL runtime backend rather than a proxy. Keeping this in `shell_sandbox`
/// means `doctor` cannot drift from `prepare_shell_sandbox_plan_with_probe`
/// (e.g. the historical Linux `bwrap` check that never matched the
/// `linux-direct-syscalls` runtime).
#[derive(Debug, Clone)]
pub struct ShellSandboxDoctor {
    /// The backend name the runtime would select on this platform.
    pub backend: &'static str,
    /// Whether that backend can actually enforce isolation right now.
    pub available: bool,
    /// Human-readable explanation for the doctor row.
    pub detail: String,
    /// Linux-specific: whether unprivileged user namespaces are available
    /// (required for `unshare(CLONE_NEWUSER|CLONE_NEWNS|CLONE_NEWNET)`).
    /// Always `None` on non-Linux platforms.
    pub linux_user_namespaces: Option<bool>,
    /// Linux-specific: Landlock ABI version exposed by the kernel,
    /// or `0` when Landlock is absent. Always `None` on non-Linux platforms.
    pub linux_landlock_abi: Option<i32>,
    /// Linux-specific: whether the seccomp BPF filter compiles successfully on
    /// this architecture. Always `None` on non-Linux platforms.
    pub linux_seccomp_available: Option<bool>,
    /// Linux-specific: whether `squeezy ask` is unavailable inside the sandboxed
    /// shell child because the seccomp profile denies AF_UNIX `socket(2)`.
    /// Always `None` on non-Linux platforms.
    pub linux_ask_socket_blocked: Option<bool>,
    /// Linux only: whether unprivileged user namespaces (`CLONE_NEWUSER`) are
    /// available. `None` on non-Linux platforms.
    pub userns: Option<bool>,
    /// Linux only: whether Landlock filesystem enforcement is available.
    /// `None` on non-Linux platforms.
    pub landlock: Option<bool>,
    /// When `available` is `false`, a short machine-readable string
    /// explaining why the backend cannot enforce isolation right now. Mirrors
    /// the prose in `detail` but under a stable key for scripts that should
    /// not scrape the human-readable string.
    pub fallback_reason: Option<String>,
}

/// Probe the active shell-sandbox backend for `doctor`.
pub fn shell_sandbox_doctor() -> ShellSandboxDoctor {
    #[cfg(target_os = "macos")]
    {
        let available = macos_sandbox_exec_supported();
        let fallback_reason = if available {
            None
        } else {
            Some("/usr/bin/sandbox-exec not found".to_string())
        };
        ShellSandboxDoctor {
            backend: "macos-sandbox-exec",
            available,
            detail: if available {
                "sandbox-exec present; deny-default Seatbelt profile enforces filesystem + network"
                    .to_string()
            } else {
                "/usr/bin/sandbox-exec not found; required mode denies, best_effort degrades"
                    .to_string()
            },
            linux_user_namespaces: None,
            linux_landlock_abi: None,
            linux_seccomp_available: None,
            linux_ask_socket_blocked: None,
            userns: None,
            landlock: None,
            fallback_reason,
        }
    }
    #[cfg(target_os = "linux")]
    {
        let userns = linux_unshare_supported();
        let landlock = linux_landlock_supported();
        let abi = linux_landlock_abi_version();
        // Read the kernel knob to surface the exact policy value.
        let userns_knob = std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone")
            .ok()
            .map(|v| v.trim().to_string());
        let userns_ns_present = std::path::Path::new("/proc/self/ns/user").exists();
        let knob_str = userns_knob.as_deref().unwrap_or("absent");
        let seccomp_ok = linux_seccomp::build_shell_filter().is_ok();
        let fallback_reason: Option<String> = match (userns, landlock) {
            (true, true) => None,
            (true, false) => Some(
                "Landlock filesystem enforcement unavailable (kernel 5.13+ required)".to_string(),
            ),
            (false, true) => Some(
                "unprivileged user namespaces disabled \
                 (kernel.unprivileged_userns_clone=0 or /proc/self/ns/user absent)"
                    .to_string(),
            ),
            (false, false) => {
                Some("neither unprivileged user namespaces nor Landlock available".to_string())
            }
        };
        let detail = match (userns, landlock) {
            (true, true) => format!(
                "unshare(CLONE_NEWUSER|NEWNS|NEWNET) + Landlock ABI {abi} + seccomp available; \
                 unprivileged_userns_clone={knob_str}, /proc/self/ns/user present={userns_ns_present}"
            ),
            (true, false) => format!(
                "user namespaces available but Landlock filesystem enforcement is not \
                 (Landlock ABI {abi}); required mode denies; \
                 unprivileged_userns_clone={knob_str}, /proc/self/ns/user present={userns_ns_present}; \
                 hint: check kernel version (Landlock requires 5.13+)"
            ),
            (false, true) => format!(
                "Landlock available (ABI {abi}) but unprivileged user namespaces are disabled; \
                 required mode denies; \
                 unprivileged_userns_clone={knob_str}, /proc/self/ns/user present={userns_ns_present}; \
                 hint: set sysctl kernel.unprivileged_userns_clone=1 or check container/seccomp policy"
            ),
            (false, false) => format!(
                "neither unprivileged user namespaces nor Landlock available (ABI {abi}); \
                 required mode denies; \
                 unprivileged_userns_clone={knob_str}, /proc/self/ns/user present={userns_ns_present}; \
                 hint: check kernel.unprivileged_userns_clone sysctl, container seccomp policy, \
                 and kernel version (Landlock requires 5.13+)"
            ),
        };
        ShellSandboxDoctor {
            backend: "linux-direct-syscalls",
            available: userns && landlock,
            detail,
            linux_user_namespaces: Some(userns),
            linux_landlock_abi: Some(abi),
            linux_seccomp_available: Some(seccomp_ok),
            // AF_UNIX is blocked by the seccomp filter only when the sandbox
            // actually runs: unshare must succeed AND the filter must compile.
            linux_ask_socket_blocked: Some(userns && seccomp_ok),
            userns: Some(userns),
            landlock: Some(landlock),
            fallback_reason,
        }
    }
    #[cfg(target_os = "windows")]
    {
        ShellSandboxDoctor {
            backend: "windows-restricted-token",
            available: true,
            detail: "Windows restricted-token sandbox: filesystem writes are enforced without \
                     admin; reads and network are not isolated. The elevated tier adds full \
                     read + write isolation plus WFP network egress control via \
                     `squeezy doctor --sandbox-setup` plus windows_sandbox_level = \"elevated\"; \
                     windows_sandbox_level = \"disabled\" uses Job Object cleanup only."
                .to_string(),
            linux_user_namespaces: None,
            linux_landlock_abi: None,
            linux_seccomp_available: None,
            linux_ask_socket_blocked: None,
            userns: None,
            landlock: None,
            fallback_reason: None,
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        ShellSandboxDoctor {
            backend: "none",
            available: false,
            detail: "no OS shell-sandbox backend is available for this platform".to_string(),
            linux_user_namespaces: None,
            linux_landlock_abi: None,
            linux_seccomp_available: None,
            linux_ask_socket_blocked: None,
            userns: None,
            landlock: None,
            fallback_reason: Some("unsupported platform".to_string()),
        }
    }
}

pub(crate) fn prepare_shell_sandbox_plan(
    command: &str,
    analysis: &ShellPermissionAnalysis,
    root: &Path,
    config: &ShellSandboxConfig,
    health: &ShellSandboxHealth,
) -> std::result::Result<ShellSandboxPlan, String> {
    prepare_shell_sandbox_plan_with_probe(
        command,
        analysis,
        root,
        config,
        health,
        macos_sandbox_exec_supported(),
        linux_unshare_supported(),
        linux_landlock_supported(),
    )
}

pub(crate) async fn apply_shell_sandbox_backend_health<F, Fut>(
    command: &str,
    config: &ShellSandboxConfig,
    health: &ShellSandboxHealth,
    plan: ShellSandboxPlan,
    probe_failure: F,
) -> std::result::Result<ShellSandboxPlan, String>
where
    F: FnOnce(ShellSandboxPlan, Duration) -> Fut,
    Fut: Future<Output = Option<String>>,
{
    let backend = plan.backend;
    if backend == "none" {
        return Ok(plan);
    }

    match health.status(backend) {
        Some(ShellSandboxBackendStatus::Available) => return Ok(plan),
        Some(ShellSandboxBackendStatus::Unavailable(reason)) => {
            if config.mode == ShellSandboxMode::Required {
                // Required mode: deny without incrementing the fallback
                // counter — a denial is not a best_effort degradation.
                return Err(format!(
                    "required shell sandbox backend {backend} unavailable: {reason}"
                ));
            }
            // Tick the fallback counter even for cached-unavailable calls so
            // the telemetry counter stays in step with every degraded shell
            // invocation (not just the first probe failure).
            let record = health.record_best_effort_fallback();
            return shell_sandbox_backend_unavailable_plan(
                command, config, backend, &reason, record,
            );
        }
        None => {}
    }

    let probe_input = plan.clone();
    if let Some(reason) = probe_failure(probe_input, SHELL_SANDBOX_BACKEND_PROBE_TIMEOUT).await {
        health.mark_unavailable(backend, reason.clone());
        if config.mode == ShellSandboxMode::Required {
            return Err(format!(
                "required shell sandbox backend {backend} unavailable: {reason}"
            ));
        }
        // First probe failure in best_effort mode: record and emit the
        // session-level latch.
        let record = health.record_best_effort_fallback();
        return shell_sandbox_backend_unavailable_plan(command, config, backend, &reason, record);
    }

    health.mark_available(backend);
    Ok(plan)
}

/// Build a best_effort fallback plan for a degraded sandbox backend.
/// The caller is responsible for having already verified that the mode is
/// NOT required (required-mode callers should return `Err` instead of
/// calling this), and for having called `record_best_effort_fallback`.
pub(crate) fn shell_sandbox_backend_unavailable_plan(
    command: &str,
    config: &ShellSandboxConfig,
    backend: &'static str,
    reason: &str,
    record: ShellSandboxFallbackRecord,
) -> std::result::Result<ShellSandboxPlan, String> {
    let fallback_reason = shell_sandbox_backend_disabled_reason(backend, reason);
    Ok(ShellSandboxPlan::direct_with_fallback_record(
        command,
        config.mode,
        config,
        Some(fallback_reason),
        Some((backend, record)),
    ))
}

pub(crate) fn shell_sandbox_backend_disabled_reason(backend: &'static str, reason: &str) -> String {
    format!(
        "shell sandbox backend {backend} disabled after health check failure: {reason}; running without OS sandbox because mode is best_effort"
    )
}

pub(crate) async fn shell_sandbox_backend_probe_failure(
    plan: ShellSandboxPlan,
    timeout: Duration,
) -> Option<String> {
    match plan.backend {
        "macos-sandbox-exec" => macos_sandbox_plan_probe_failure(plan, timeout).await,
        "windows-restricted-token" | "windows-elevated" => {
            windows_sandbox_plan_probe_failure(plan, timeout).await
        }
        // Linux support is already probed before this point via unshare and
        // Landlock capability checks; a second process probe would add latency
        // without exercising the same pre_exec path.
        "linux-direct-syscalls" => None,
        _ => None,
    }
}

#[cfg(target_os = "macos")]
async fn macos_sandbox_plan_probe_failure(
    plan: ShellSandboxPlan,
    timeout: Duration,
) -> Option<String> {
    let mut args = plan.args.clone();
    let Some(command_arg) = args.last_mut() else {
        return Some(format!(
            "shell sandbox backend {} probe could not build command",
            plan.backend
        ));
    };
    *command_arg = "true".to_string();

    let mut child = match tokio::process::Command::new(&plan.program)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            return Some(format!(
                "shell sandbox backend {} probe failed to start: {err}",
                plan.backend
            ));
        }
    };

    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) if status.success() => None,
        Ok(Ok(status)) => Some(shell_sandbox_backend_probe_status_reason(
            plan.backend,
            &status,
        )),
        Ok(Err(err)) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Some(format!(
                "shell sandbox backend {} probe wait failed: {err}",
                plan.backend
            ))
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Some(format!(
                "shell sandbox backend {} probe timed out after {} ms",
                plan.backend,
                timeout.as_millis()
            ))
        }
    }
}

#[cfg(not(target_os = "macos"))]
async fn macos_sandbox_plan_probe_failure(
    _plan: ShellSandboxPlan,
    _timeout: Duration,
) -> Option<String> {
    None
}

#[cfg(target_os = "windows")]
async fn windows_sandbox_plan_probe_failure(
    mut plan: ShellSandboxPlan,
    timeout: Duration,
) -> Option<String> {
    let Some(command_arg) = plan.args.last_mut() else {
        return Some(format!(
            "shell sandbox backend {} probe could not build command",
            plan.backend
        ));
    };
    *command_arg = "echo squeezy_sandbox_probe".to_string();

    let Some(workdir) = plan.filesystem_write_roots.first().cloned() else {
        return Some(format!(
            "shell sandbox backend {} probe has no writable workspace root",
            plan.backend
        ));
    };
    let spec = squeezy_win_sandbox::WinSandboxSpec {
        token_mode: squeezy_win_sandbox::WinTokenMode::WritableRootsCapability,
        writable_roots: plan
            .filesystem_write_roots
            .iter()
            .map(squeezy_win_sandbox::WinWritableRoot::new)
            .collect(),
        read_roots: plan.filesystem_read_roots.clone(),
        deny_read_paths: Vec::new(),
        protected_metadata_names: Vec::new(),
        sensitive_path_patterns: Vec::new(),
        network: squeezy_win_sandbox::WinNetwork::Unenforced,
        state_dir: squeezy_core::default_win_sandbox_state_dir(),
    };
    let mut argv = Vec::with_capacity(1 + plan.args.len());
    argv.push(plan.program.clone());
    argv.extend(plan.args.iter().cloned());
    // The probe deliberately inherits the host environment so the backend has
    // the same `PATH`, `SystemRoot`, and adjacent variables a real shell would see.
    let env: HashMap<String, String> = std::env::vars().collect();
    let spawned = if plan.backend == "windows-elevated" {
        squeezy_win_sandbox::spawn_elevated(&spec, &argv, &workdir, &env, false)
    } else {
        squeezy_win_sandbox::spawn_restricted_token(&spec, &argv, &workdir, &env, false)
    };
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => {
            return Some(format!(
                "shell sandbox backend {} probe failed to start: {err}",
                plan.backend
            ));
        }
    };

    let timeout = timeout.max(Duration::from_secs(5));
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) if status.success() => None,
        Ok(Ok(status)) => Some(shell_sandbox_backend_probe_status_reason(
            plan.backend,
            &status,
        )),
        Ok(Err(err)) => {
            child.kill();
            let _ = child.wait().await;
            Some(format!(
                "shell sandbox backend {} probe wait failed: {err}",
                plan.backend
            ))
        }
        Err(_) => {
            child.kill();
            let _ = child.wait().await;
            Some(format!(
                "shell sandbox backend {} probe timed out after {} ms",
                plan.backend,
                timeout.as_millis()
            ))
        }
    }
}

#[cfg(not(target_os = "windows"))]
async fn windows_sandbox_plan_probe_failure(
    _plan: ShellSandboxPlan,
    _timeout: Duration,
) -> Option<String> {
    None
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn shell_sandbox_backend_probe_status_reason(
    backend: &'static str,
    status: &std::process::ExitStatus,
) -> String {
    if let Some(code) = status.code() {
        return format!("shell sandbox backend {backend} probe exited with code {code}");
    }
    if let Some(signal) = shell_exit_signal(Some(status)) {
        return format!("shell sandbox backend {backend} probe terminated by signal {signal}");
    }
    format!("shell sandbox backend {backend} probe ended without an exit code")
}

#[allow(unused_variables)]
/// Writable roots the Windows sandbox grants: the workspace, any configured
/// write roots, and the per-user temp dirs. Mirrors `shell_writable_roots`
/// (macOS/Linux) for the Windows tiers; deduplicated, order-preserving.
#[cfg(target_os = "windows")]
fn windows_writable_roots(root: &Path, config: &ShellSandboxConfig) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = vec![root.to_path_buf()];
    for write_root in &config.write_roots {
        if !roots.contains(write_root) {
            roots.push(write_root.clone());
        }
    }
    for var in ["TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(var) {
            let path = PathBuf::from(value);
            if !roots.contains(&path) {
                roots.push(path);
            }
        }
    }
    roots
}

/// Return the shell display name for inclusion in Windows plan metadata.
/// Always returns `None` on non-Windows so the field stays absent there.
fn windows_shell_display_name(shell: &ShellProgram) -> Option<String> {
    #[cfg(windows)]
    {
        Some(shell.display_name.clone())
    }
    #[cfg(not(windows))]
    {
        let _ = shell;
        None
    }
}

/// Build a restricted-token-tier plan: filesystem *writes* (and write
/// carve-outs) are enforced via the restricted token + on-disk ACLs, but reads
/// and network are not enforceable on this tier (a `WRITE_RESTRICTED` token
/// does not gate reads, and egress cannot be scoped without a distinct user).
/// The posture strings (`enforced_writes_only` / `not_enforced`) report this
/// honestly. Reused as the best-effort fallback when the elevated tier is
/// selected but not yet provisioned.
#[cfg(target_os = "windows")]
fn windows_restricted_plan(
    command: &str,
    config: &ShellSandboxConfig,
    root: &Path,
    required: bool,
    fallback_reason: Option<String>,
) -> ShellSandboxPlan {
    let shell = ShellProgram::for_windows_restricted_command(command);
    let selected_shell = Some(shell.display_name.clone());
    ShellSandboxPlan {
        program: shell.program,
        args: shell.args,
        backend: "windows-restricted-token",
        mode: config.mode.as_str(),
        network: "not_enforced",
        filesystem: "enforced_writes_only",
        required,
        configured_read_roots: config.read_roots.clone(),
        configured_write_roots: config.write_roots.clone(),
        filesystem_read_roots: Vec::new(),
        filesystem_write_roots: windows_writable_roots(root, config),
        fallback_reason,
        best_effort_fallback: None,
        selected_shell,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_shell_sandbox_plan_with_probe(
    command: &str,
    analysis: &ShellPermissionAnalysis,
    root: &Path,
    config: &ShellSandboxConfig,
    health: &ShellSandboxHealth,
    macos_sandbox_exec_available: bool,
    linux_unshare_available: bool,
    linux_landlock_available: bool,
) -> std::result::Result<ShellSandboxPlan, String> {
    // Fail fast if SQUEEZY_SHELL names a shell that cannot be resolved on
    // this host (e.g. `gitbash` when Git Bash is absent on Windows).  This
    // produces a clear tool error instead of a confusing spawn failure later.
    ShellProgram::validate_override()?;

    // Each probe result is consumed only by its own platform's backend branch
    // below; reference the others so the non-matching targets (and the Windows
    // CI `clippy -D warnings` gate) don't flag them as unused.
    #[cfg(not(target_os = "macos"))]
    let _ = (macos_sandbox_exec_available, health);
    #[cfg(not(target_os = "linux"))]
    let _ = (linux_unshare_available, linux_landlock_available);

    if config.mode == ShellSandboxMode::Off {
        return Ok(ShellSandboxPlan::direct(
            command,
            ShellSandboxMode::Off,
            config,
        ));
    }
    if config.mode == ShellSandboxMode::External {
        return Ok(ShellSandboxPlan::external(command, config));
    }

    let required = config.mode == ShellSandboxMode::Required;
    // The sandbox-level network posture has THREE distinct states:
    //   - "allowed_approved": classified network + user opted into
    //     `allow_when_approved`; the sandbox opens its network namespace.
    //   - "denied_classified": classified network + default
    //     `deny_by_default`; the permission layer may still allow the
    //     command to RUN, but the sandbox keeps network closed so a
    //     misclassified target or a follow-on system() call can't reach
    //     out unnoticed.
    //   - "denied": non-network classification; sandbox always denies.
    let network = match (config.network, analysis.network) {
        (ShellSandboxNetworkPolicy::AllowWhenApproved, true) => "allowed_approved",
        (ShellSandboxNetworkPolicy::DenyByDefault, true) => "denied_classified",
        _ => "denied",
    };
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let fallback_reason: Option<String>;
    // The Windows branch builds its own plans and never consults
    // `fallback_reason`; only the generic non-(macOS|Linux|Windows) tail does.
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let fallback_reason: Option<String> = None;

    #[cfg(target_os = "macos")]
    {
        if macos_sandbox_exec_available {
            // Only `allowed_approved` means the sandbox should actually open
            // its network namespace. Both `denied` (non-network command) and
            // `denied_classified` (network-classified but policy is
            // deny_by_default) must keep the sandbox network-closed.
            let allow_network = network == "allowed_approved";
            let profile = {
                let cache_idx = usize::from(allow_network);
                health.sbpl_profile_cache[cache_idx]
                    .get_or_init(|| macos_shell_sandbox_profile(root, config, allow_network))
                    .clone()
            };
            return Ok(ShellSandboxPlan {
                program: "/usr/bin/sandbox-exec".to_string(),
                args: vec![
                    "-p".to_string(),
                    profile,
                    "sh".to_string(),
                    "-lc".to_string(),
                    command.to_string(),
                ],
                backend: "macos-sandbox-exec",
                mode: config.mode.as_str(),
                network,
                filesystem: "enforced",
                required,
                configured_read_roots: config.read_roots.clone(),
                configured_write_roots: config.write_roots.clone(),
                filesystem_read_roots: Vec::new(),
                filesystem_write_roots: Vec::new(),
                fallback_reason: None,
                best_effort_fallback: None,
                selected_shell: None,
            });
        }
        if required {
            return Err(
                "required shell sandbox unavailable: /usr/bin/sandbox-exec not found or cannot apply profiles"
                    .to_string(),
            );
        }
        fallback_reason = Some(
            "macos sandbox-exec unavailable; running without OS sandbox because mode is best_effort"
                .to_string(),
        );
    }

    #[cfg(target_os = "linux")]
    {
        // Probe whether unshare can actually be applied as the current
        // user. If the kernel forbids it (e.g. unprivileged_userns_clone=0
        // or seccomp policy in the container), required mode must fail
        // closed at sandbox-prepare time rather than silently exit 1
        // after fork.
        if !linux_unshare_available {
            if required {
                return Err(format!(
                    "required shell sandbox unavailable: linux unshare(CLONE_NEWUSER|CLONE_NEWNS{}) not permitted on this host",
                    if network == "denied" {
                        "|CLONE_NEWNET"
                    } else {
                        ""
                    }
                ));
            }
            fallback_reason = Some(
                "linux unshare unavailable; running without OS sandbox because mode is best_effort"
                    .to_string(),
            );
        } else {
            // In required mode, verify that every user-configured root
            // actually exists at plan time. Optional default roots are
            // checked lazily in linux_shell_read_roots; user-configured
            // roots are validated here so that a missing path causes an
            // immediate, clear error rather than a silently shorter
            // Landlock allowlist in the audit log.
            if required && linux_landlock_available {
                for root_path in config.read_roots.iter().chain(config.write_roots.iter()) {
                    if !root_path.exists() {
                        return Err(format!(
                            "required shell sandbox: configured root {} does not exist at spawn time",
                            root_path.display()
                        ));
                    }
                }
            }
            let filesystem = if linux_landlock_available {
                "enforced"
            } else if required {
                return Err("required shell sandbox unavailable: linux Landlock filesystem enforcement unavailable".to_string());
            } else {
                "best_effort_unavailable"
            };
            let shell_program = config
                .linux_shell
                .as_deref()
                .unwrap_or("/bin/sh")
                .to_string();
            return Ok(ShellSandboxPlan {
                program: shell_program,
                args: vec!["-lc".to_string(), command.to_string()],
                backend: "linux-direct-syscalls",
                mode: config.mode.as_str(),
                network,
                filesystem,
                required,
                configured_read_roots: config.read_roots.clone(),
                configured_write_roots: config.write_roots.clone(),
                filesystem_read_roots: if linux_landlock_available {
                    linux_shell_read_roots(root, config)
                } else {
                    Vec::new()
                },
                filesystem_write_roots: if linux_landlock_available {
                    shell_writable_roots(root, config)
                } else {
                    Vec::new()
                },
                fallback_reason: None,
                best_effort_fallback: None,
                selected_shell: None,
            });
        }
    }

    #[cfg(target_os = "windows")]
    {
        use squeezy_core::WindowsSandboxLevel;

        match config.windows_sandbox_level {
            WindowsSandboxLevel::Disabled => {
                if required {
                    return Err(
                        "required shell sandbox unavailable on windows: windows_sandbox_level = \"disabled\" provides no filesystem/network isolation; set windows_sandbox_level = \"restricted_token\" (or use mode = \"best_effort\"/\"external\")"
                            .to_string(),
                    );
                }
                let shell = ShellProgram::for_command(command);
                let selected_shell = Some(shell.display_name.clone());
                Ok(ShellSandboxPlan {
                    program: shell.program,
                    args: shell.args,
                    backend: "windows-job-object",
                    mode: config.mode.as_str(),
                    network: if network == "denied" {
                        "denied_best_effort"
                    } else {
                        network
                    },
                    filesystem: "best_effort_unavailable",
                    required: false,
                    configured_read_roots: config.read_roots.clone(),
                    configured_write_roots: config.write_roots.clone(),
                    filesystem_read_roots: Vec::new(),
                    filesystem_write_roots: Vec::new(),
                    fallback_reason: Some(
                        "windows: windows_sandbox_level=disabled; process-tree cleanup via Job Object only; no FS/network isolation".to_string(),
                    ),
                    best_effort_fallback: None,
                    selected_shell,
                })
            }
            WindowsSandboxLevel::Elevated
                if squeezy_win_sandbox::elevated_setup_is_complete(
                    &crate::win_sandbox_spec::win_state_dir(),
                ) =>
            {
                let shell = ShellProgram::for_command(command);
                let selected_shell = Some(shell.display_name.clone());
                Ok(ShellSandboxPlan {
                    program: shell.program,
                    args: shell.args,
                    backend: "windows-elevated",
                    mode: config.mode.as_str(),
                    // Network is genuinely enforced on the elevated tier: the
                    // offline identity carries WFP egress-block filters; an
                    // approved-network command runs under the online identity.
                    network,
                    filesystem: "enforced",
                    required,
                    configured_read_roots: config.read_roots.clone(),
                    configured_write_roots: config.write_roots.clone(),
                    filesystem_read_roots: config.read_roots.clone(),
                    filesystem_write_roots: windows_writable_roots(root, config),
                    fallback_reason: None,
                    best_effort_fallback: None,
                    selected_shell,
                })
            }
            WindowsSandboxLevel::Elevated => {
                // Selected but not provisioned. Required fails closed;
                // best-effort degrades to the restricted-token tier (no setup
                // needed) and records why reads + network are not enforced.
                if required {
                    return Err(
                        "required shell sandbox unavailable on windows: elevated tier is not provisioned; run `squeezy doctor --sandbox-setup` once (UAC), or set windows_sandbox_level = \"restricted_token\""
                            .to_string(),
                    );
                }
                Ok(windows_restricted_plan(
                    command,
                    config,
                    root,
                    false,
                    Some(
                        "windows: elevated tier not provisioned (run `squeezy doctor --sandbox-setup`); fell back to restricted-token — filesystem writes enforced, reads + network not enforced".to_string(),
                    ),
                ))
            }
            WindowsSandboxLevel::RestrictedToken => Ok(windows_restricted_plan(
                command, config, root, required, None,
            )),
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            if required {
                return Err(format!(
                    "required shell sandbox unavailable on {}",
                    std::env::consts::OS
                ));
            }
        }

        let plan =
            ShellSandboxPlan::direct_with_fallback(command, config.mode, config, fallback_reason);
        // On Linux, respect the configured shell in the degraded path so that
        // a project relying on Bash syntax or Fish/Zsh aliases does not
        // silently switch to /bin/sh when the sandbox falls back.
        #[cfg(target_os = "linux")]
        let plan = {
            let mut plan = plan;
            if let Some(linux_shell) = &config.linux_shell {
                plan.program = linux_shell.clone();
                plan.args = vec!["-lc".to_string(), command.to_string()];
            }
            plan
        };
        Ok(plan)
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn macos_shell_sandbox_profile(
    root: &Path,
    config: &ShellSandboxConfig,
    allow_network: bool,
) -> String {
    let mut profile = String::from("(version 1)\n(deny default)\n");
    // Process-level capabilities every build/run/test needs.
    profile.push_str("(allow process-exec)\n");
    profile.push_str("(allow process-fork)\n");
    profile.push_str("(allow signal (target self))\n");
    profile.push_str("(allow sysctl-read)\n");
    profile.push_str("(allow mach-lookup)\n");
    profile.push_str("(allow ipc-posix-shm)\n");
    profile.push_str("(allow iokit-open)\n");
    profile.push_str("(allow system-socket)\n");
    profile.push_str("(allow file-read-metadata)\n");
    // Reads from system / toolchain prefixes: required so compilers,
    // shells, dynamic linker, and certificate stores can do their job.
    let mut read_roots = macos_read_roots();
    read_roots.extend(config.read_roots.iter().cloned());
    read_roots.extend(config.write_roots.iter().cloned());
    read_roots.sort();
    read_roots.dedup();
    for path in read_roots {
        profile.push_str(&format!(
            "(allow file-read* (subpath {}))\n",
            sandbox_profile_string(&path.display().to_string())
        ));
    }
    // Read+write inside the workspace, tmp dirs, and toolchain caches.
    let mut write_roots = shell_writable_roots(root, config);
    write_roots.sort();
    write_roots.dedup();
    for path in write_roots {
        let escaped = sandbox_profile_string(&path.display().to_string());
        profile.push_str(&format!("(allow file-read* (subpath {escaped}))\n"));
        if config.protected_metadata_names.is_empty() {
            profile.push_str(&format!("(allow file-write* (subpath {escaped}))\n"));
        } else {
            profile.push_str(&format!(
                "(allow file-write* (require-all (subpath {escaped})"
            ));
            for name in &config.protected_metadata_names {
                let protected = sandbox_profile_string(&path.join(name).display().to_string());
                profile.push_str(&format!(" (require-not (subpath {protected}))"));
            }
            profile.push_str("))\n");
        }
    }
    // Sensitive paths get an EXPLICIT deny on top of the default deny so
    // even if a future allow rule widens reads, these subpaths stay
    // blocked.
    let mut denied_paths = sensitive_absolute_paths(root, config);
    denied_paths.sort();
    denied_paths.dedup();
    for path in denied_paths {
        profile.push_str(&format!(
            "(deny file-read* file-write* (subpath {}))\n",
            sandbox_profile_string(&path.display().to_string())
        ));
    }
    if allow_network {
        profile.push_str("(allow network*)\n");
    } else {
        // When network is denied we narrow AF_UNIX to an explicit
        // allowlist rather than allowing every local socket. Without an
        // allowlist a sandboxed shell can reach pulseaudio, ssh-agent,
        // WindowServer, etc. via `nc -U`. Each allowed entry is matched
        // as a path subpath so prefixes like `/private/tmp/agent.sock`
        // cover sockets created beneath them.
        // Use the per-config allowlist (promoted from the old empty constant).
        for entry in &config.macos_socket_domain_allowlist {
            let escaped = sandbox_profile_string(entry);
            profile.push_str(&format!(
                "(allow network* (local unix-socket (subpath {escaped})))\n"
            ));
            profile.push_str(&format!(
                "(allow network-inbound (local unix-socket (subpath {escaped})))\n"
            ));
        }
    }
    profile
}

// MACOS_AF_UNIX_ALLOWLIST has been promoted to `ShellSandboxConfig::macos_socket_domain_allowlist`
// with conservative defaults (empty) and per-workspace config support.

/// Read-only roots every shell needs to look at: system libraries, the
/// dynamic linker, certificate stores, the toolchain prefix, and the user's
/// rustup / cargo prefixes. We add the prefixes as reads here AND as
/// writable roots below so cargo can read its registry index even when
/// not invoked under `cargo build`.
#[cfg(target_os = "macos")]
fn macos_read_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = [
        "/usr",
        "/bin",
        "/sbin",
        "/System",
        "/Library",
        "/private/etc",
        "/private/var/db",
        "/private/var/folders",
        "/opt",
        "/dev/null",
        "/dev/zero",
        "/dev/random",
        "/dev/urandom",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    // Toolchain prefixes the user may have configured.
    for name in ["CARGO_HOME", "RUSTUP_HOME"] {
        if let Some(path) = env::var_os(name).map(PathBuf::from) {
            roots.push(path);
        }
    }
    // Default toolchain locations under $HOME.
    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join(".cargo"));
        roots.push(home.join(".rustup"));
    }
    roots
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn shell_writable_roots(root: &Path, config: &ShellSandboxConfig) -> Vec<PathBuf> {
    let mut roots = vec![
        root.to_path_buf(),
        PathBuf::from("/tmp"),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/private/var/folders"),
    ];
    for name in ["TMPDIR", "TEMP", "TMP", "CARGO_HOME", "RUSTUP_HOME"] {
        if let Some(path) = env::var_os(name).map(PathBuf::from) {
            roots.push(path);
        }
    }
    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        // The toolchain writes through `cargo build` / `cargo test` etc.;
        // adding these by default avoids breaking the canonical use case
        // when `mode = "required"`.
        roots.push(home.join(".cargo"));
        roots.push(home.join(".rustup"));
    }
    roots.extend(config.write_roots.iter().cloned());
    roots.sort();
    roots.dedup();
    roots
}

#[cfg(target_os = "linux")]
fn linux_shell_read_roots(root: &Path, config: &ShellSandboxConfig) -> Vec<PathBuf> {
    // Always-present system paths required for shell and compiler operation.
    let mut roots: Vec<PathBuf> = ["/usr", "/bin", "/sbin", "/lib", "/etc"]
        .iter()
        .map(PathBuf::from)
        .collect();
    // Optional system paths: only included when they exist at plan time.
    // /lib64 is absent on 32-bit and some musl targets; /opt is absent on
    // many minimal containers; /nix/store is Nix-only.
    for path in ["/lib64", "/opt", "/nix/store"] {
        let p = PathBuf::from(path);
        if p.exists() {
            roots.push(p);
        }
    }
    // Narrow device access: only the non-symlink device nodes a sandboxed
    // shell legitimately uses. Symlinks into /proc/self/fd (like /dev/fd,
    // /dev/stdin, /dev/stdout, /dev/stderr) are excluded because they
    // follow into the child's /proc namespace and can cause Landlock
    // rule-add failures in the new namespace context. The broad /dev
    // allowlist can expose readable device paths that build/test commands
    // do not need.
    for path in [
        "/dev/null",
        "/dev/zero",
        "/dev/random",
        "/dev/urandom",
        "/dev/full",
        "/dev/tty",
        "/dev/pts",
        "/dev/ptmx",
    ] {
        let p = PathBuf::from(path);
        if p.exists() {
            roots.push(p);
        }
    }
    // Narrow /proc access: only the paths a sandboxed shell needs at
    // runtime. Symlinks within /proc (like /proc/mounts → /proc/self/mounts
    // and /proc/net → /proc/self/net) are excluded because they resolve into
    // namespace-specific paths that may behave differently after unshare.
    // /proc/self covers the child's own process entries. Broad /proc access
    // can expose process metadata and host/container topology.
    for path in [
        "/proc/self",
        "/proc/sys",
        "/proc/version",
        "/proc/cpuinfo",
        "/proc/meminfo",
        "/proc/filesystems",
    ] {
        let p = PathBuf::from(path);
        if p.exists() {
            roots.push(p);
        }
    }
    for name in ["CARGO_HOME", "RUSTUP_HOME"] {
        if let Some(path) = env::var_os(name).map(PathBuf::from) {
            roots.push(path);
        }
    }
    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join(".cargo"));
        roots.push(home.join(".rustup"));
    }
    roots.push(root.to_path_buf());
    roots.extend(config.read_roots.iter().cloned());
    roots.extend(config.write_roots.iter().cloned());
    roots.sort();
    roots.dedup();
    roots
}

/// Resolve the list of absolute paths the macOS sandbox profile should
/// explicitly deny on top of the (deny default) base. Only the macOS
/// profile generator calls this; gated to avoid dead-code warnings on
/// Linux and other targets where no sandbox-exec profile is generated.
#[cfg(target_os = "macos")]
fn sensitive_absolute_paths(root: &Path, config: &ShellSandboxConfig) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for pattern in &config.sensitive_path_patterns {
        let base = sensitive_pattern_base(pattern);
        if base.is_empty() {
            continue;
        }
        paths.push(root.join(&base));
        if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
            paths.push(home.join(&base));
        }
        for allowed_root in config.read_roots.iter().chain(config.write_roots.iter()) {
            paths.push(allowed_root.join(&base));
        }
    }
    paths
}

#[cfg(target_os = "macos")]
fn sandbox_profile_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Recognises the on-process signals that the sandbox backend itself
/// failed to apply (as opposed to the user's command failing). Used in
/// `mode = "required"` to deny the call rather than silently letting it
/// run unsandboxed.
pub(crate) fn shell_sandbox_runtime_unavailable(
    plan: &ShellSandboxPlan,
    exit_code: Option<i32>,
    stderr: &str,
) -> bool {
    shell_sandbox_runtime_unavailable_with_probe(plan, exit_code, stderr, linux_unshare_supported())
}

pub(crate) fn shell_sandbox_runtime_unavailable_with_probe(
    plan: &ShellSandboxPlan,
    exit_code: Option<i32>,
    stderr: &str,
    linux_unshare_available: bool,
) -> bool {
    match plan.backend {
        "macos-sandbox-exec" => {
            // sandbox-exec returns 71 with a `sandbox_apply` message when
            // the kernel refuses to apply the SBPL profile.
            exit_code == Some(71) && stderr.contains("sandbox_apply")
        }
        "linux-direct-syscalls" => {
            // The pre_exec hook returns Err with an EPERM/EINVAL when
            // unshare fails after a successful spawn handshake. Tokio's
            // child reports this as a Unix `_exit(1)`/wait status with
            // empty stdout/stderr; we can't distinguish that perfectly
            // from a legitimate `exit 1`. Fall back to a probe: re-check
            // the supported-flag at the parent level, and report
            // unavailable if the kernel no longer supports unshare.
            !linux_unshare_available && exit_code == Some(1) && stderr.is_empty()
        }
        _ => false,
    }
}

pub(crate) fn shell_sandbox_best_effort_fallback_reason(
    sandbox_plan: &ShellSandboxPlan,
    run: &ShellRunOutcome,
) -> Option<String> {
    shell_sandbox_runtime_fallback_reason(sandbox_plan, run)
}

pub(crate) fn shell_sandbox_runtime_fallback_reason(
    sandbox_plan: &ShellSandboxPlan,
    run: &ShellRunOutcome,
) -> Option<String> {
    if sandbox_plan.required || sandbox_plan.backend == "none" || run.timed_out {
        return None;
    }

    let exit_code = run.exit_status.as_ref().and_then(|status| status.code());
    let stderr = String::from_utf8_lossy(&run.stderr_bytes);
    if shell_sandbox_runtime_unavailable(sandbox_plan, exit_code, &stderr) {
        return Some(format!(
            "shell sandbox backend {} failed at runtime; retried without OS sandbox because mode is best_effort",
            sandbox_plan.backend
        ));
    }

    None
}

pub(crate) fn configure_shell_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = command;
    }
}

pub(crate) fn configure_linux_shell_sandbox(command: &mut Command, plan: &ShellSandboxPlan) {
    #[cfg(target_os = "linux")]
    if plan.backend == "linux-direct-syscalls" {
        let deny_network = plan.network == "denied";
        let enforce_filesystem = plan.filesystem == "enforced";
        let read_roots = plan.filesystem_read_roots.clone();
        let write_roots = plan.filesystem_write_roots.clone();
        // `Command::process_group(0)` already arranges a `setpgid(0, 0)` in
        // the child's pre_exec, so we don't duplicate it here. We focus on
        // the namespace unshare, which is the additional isolation step.
        // CLONE_NEWUSER + uid_map is required for an unprivileged process
        // to call unshare(CLONE_NEWNS) on stock distros; we fall back to a
        // single-step unshare if user-namespace setup is forbidden so that
        // best-effort mode does not hard-fail on every call.
        unsafe {
            command.pre_exec(move || {
                linux_unshare_pre_exec(deny_network)?;
                if enforce_filesystem {
                    linux_landlock_restrict(&read_roots, &write_roots)?;
                }
                // The seccomp filter is the last step before exec: it must
                // not block any of the prctl/unshare/landlock setup, and it
                // applies to the upcoming `execve` and everything inherited
                // by the shell. We set `PR_SET_NO_NEW_PRIVS` here as a
                // safety net in case the landlock branch was skipped — the
                // kernel requires NNP for an unprivileged seccomp install.
                linux_seccomp::install_shell_filter()?;
                Ok(())
            });
        }
    }

    #[cfg(not(target_os = "linux"))]
    let _ = (command, plan);
}

#[cfg(target_os = "linux")]
fn linux_unshare_pre_exec(deny_network: bool) -> std::io::Result<()> {
    // Capture the parent's uid/gid before any namespace switch.
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    // Preferred path: open a user namespace first so the subsequent mount
    // and network namespace creation are allowed without CAP_SYS_ADMIN.
    let mut flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNS;
    if deny_network {
        flags |= libc::CLONE_NEWNET;
    }
    if unsafe { libc::unshare(flags) } == 0 {
        // Best effort: drop the inherited setgroups capability and map our
        // uid/gid into the new user namespace. If any of these writes fail
        // (e.g. /proc not yet mounted), continue — the sandbox is still in
        // place; the only effect is that uid/gid inside the namespace look
        // unmapped.
        let _ = linux_write_proc("/proc/self/setgroups", b"deny");
        let _ = linux_write_proc("/proc/self/uid_map", format!("0 {uid} 1").as_bytes());
        let _ = linux_write_proc("/proc/self/gid_map", format!("0 {gid} 1").as_bytes());
        return Ok(());
    }

    // Fallback path: try the privileged form. Will succeed in containers
    // launched with CAP_SYS_ADMIN, fail with EPERM otherwise.
    let mut fallback = libc::CLONE_NEWNS;
    if deny_network {
        fallback |= libc::CLONE_NEWNET;
    }
    if unsafe { libc::unshare(fallback) } == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}

#[cfg(target_os = "linux")]
fn linux_write_proc(path: &str, contents: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    file.write_all(contents)
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct LandlockRulesetAttr {
    handled_access_fs: u64,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct LandlockPathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

#[cfg(target_os = "linux")]
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
#[cfg(target_os = "linux")]
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;
#[cfg(target_os = "linux")]
const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
#[cfg(target_os = "linux")]
const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
#[cfg(target_os = "linux")]
const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
#[cfg(target_os = "linux")]
const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
#[cfg(target_os = "linux")]
const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
#[cfg(target_os = "linux")]
const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
#[cfg(target_os = "linux")]
const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
#[cfg(target_os = "linux")]
const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
#[cfg(target_os = "linux")]
const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
#[cfg(target_os = "linux")]
const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
#[cfg(target_os = "linux")]
const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
#[cfg(target_os = "linux")]
const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
#[cfg(target_os = "linux")]
const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
#[cfg(target_os = "linux")]
const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13;
#[cfg(target_os = "linux")]
const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14;
#[cfg(target_os = "linux")]
const LANDLOCK_ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;

#[cfg(target_os = "linux")]
pub(crate) fn linux_landlock_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| linux_landlock_abi_version() > 0)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn linux_landlock_supported() -> bool {
    false
}

#[cfg(target_os = "linux")]
fn linux_landlock_abi_version() -> i32 {
    static ABI: OnceLock<i32> = OnceLock::new();
    *ABI.get_or_init(linux_landlock_abi_version_uncached)
}

#[cfg(target_os = "linux")]
fn linux_landlock_abi_version_uncached() -> i32 {
    let version = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if version <= 0 { 0 } else { version as i32 }
}

#[cfg(target_os = "linux")]
fn linux_landlock_restrict(read_roots: &[PathBuf], write_roots: &[PathBuf]) -> std::io::Result<()> {
    let abi = linux_landlock_abi_version();
    if abi <= 0 {
        return Err(std::io::Error::other("Landlock is unavailable"));
    }
    let handled_access_fs = linux_landlock_handled_access(abi);
    let ruleset_attr = LandlockRulesetAttr { handled_access_fs };
    let ruleset_fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &ruleset_attr as *const LandlockRulesetAttr,
            std::mem::size_of::<LandlockRulesetAttr>(),
            0u32,
        )
    };
    if ruleset_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let ruleset_fd = ruleset_fd as libc::c_int;
    let add_result = (|| {
        let read_access = linux_landlock_read_access(handled_access_fs);
        let write_access = linux_landlock_write_access(handled_access_fs);
        for root in read_roots {
            linux_landlock_add_path_rule(ruleset_fd, root, read_access)?;
        }
        for root in write_roots {
            linux_landlock_add_path_rule(ruleset_fd, root, write_access)?;
        }
        Ok(())
    })();
    if let Err(err) = add_result {
        unsafe {
            libc::close(ruleset_fd);
        }
        return Err(err);
    }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        let err = std::io::Error::last_os_error();
        unsafe {
            libc::close(ruleset_fd);
        }
        return Err(err);
    }
    let restrict_result =
        unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset_fd, 0u32) };
    // Save errno immediately before any other syscall can clobber it.
    let restrict_err = if restrict_result < 0 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        libc::close(ruleset_fd);
    }
    if let Some(err) = restrict_err {
        return Err(err);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_landlock_add_path_rule(
    ruleset_fd: libc::c_int,
    path: &Path,
    allowed_access: u64,
) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    if !path.exists() {
        return Ok(());
    }
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("sandbox root contains NUL byte"))?;
    let parent_fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if parent_fd < 0 {
        // Silently skip paths that cannot be opened in the current namespace
        // context (e.g., proc symlinks after CLONE_NEWNS / CLONE_NEWNET).
        // A missing rule means the path is simply not in the allowlist,
        // which is the stricter default — the sandbox remains active.
        return Ok(());
    }
    let path_beneath = LandlockPathBeneathAttr {
        allowed_access,
        parent_fd,
    };
    let result = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset_fd,
            LANDLOCK_RULE_PATH_BENEATH,
            &path_beneath as *const LandlockPathBeneathAttr,
            0u32,
        )
    };
    unsafe {
        libc::close(parent_fd);
    }
    if result < 0 {
        // Silently skip paths whose type is not compatible with Landlock's
        // RULE_PATH_BENEATH (e.g., special device files or virtual-fs paths
        // on some kernel configurations). A failed rule means the path is
        // denied by default, which is the stricter behavior.
        return Ok(());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_landlock_handled_access(abi: i32) -> u64 {
    let mut access = LANDLOCK_ACCESS_FS_EXECUTE
        | LANDLOCK_ACCESS_FS_WRITE_FILE
        | LANDLOCK_ACCESS_FS_READ_FILE
        | LANDLOCK_ACCESS_FS_READ_DIR
        | LANDLOCK_ACCESS_FS_REMOVE_DIR
        | LANDLOCK_ACCESS_FS_REMOVE_FILE
        | LANDLOCK_ACCESS_FS_MAKE_CHAR
        | LANDLOCK_ACCESS_FS_MAKE_DIR
        | LANDLOCK_ACCESS_FS_MAKE_REG
        | LANDLOCK_ACCESS_FS_MAKE_SOCK
        | LANDLOCK_ACCESS_FS_MAKE_FIFO
        | LANDLOCK_ACCESS_FS_MAKE_BLOCK
        | LANDLOCK_ACCESS_FS_MAKE_SYM;
    if abi >= 2 {
        access |= LANDLOCK_ACCESS_FS_REFER;
    }
    if abi >= 3 {
        access |= LANDLOCK_ACCESS_FS_TRUNCATE;
    }
    if abi >= 5 {
        access |= LANDLOCK_ACCESS_FS_IOCTL_DEV;
    }
    access
}

#[cfg(target_os = "linux")]
fn linux_landlock_read_access(handled_access_fs: u64) -> u64 {
    handled_access_fs
        & (LANDLOCK_ACCESS_FS_EXECUTE
            | LANDLOCK_ACCESS_FS_READ_FILE
            | LANDLOCK_ACCESS_FS_READ_DIR
            | LANDLOCK_ACCESS_FS_IOCTL_DEV)
}

#[cfg(target_os = "linux")]
fn linux_landlock_write_access(handled_access_fs: u64) -> u64 {
    handled_access_fs
}

/// Probe whether the kernel currently permits unprivileged user-namespace
/// creation. We do this from the parent process by reading the well-known
/// sysctl knob; this is the same signal that controls whether the eventual
/// child `unshare(CLONE_NEWUSER|...)` will succeed. If the sysctl is
/// missing (older kernels, namespaces unsupported altogether) we treat
/// that as "not supported" so required mode denies pre-spawn instead of
/// silently failing inside the child.
#[cfg(target_os = "linux")]
pub(crate) fn linux_unshare_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(linux_unshare_supported_uncached)
}

#[cfg(target_os = "linux")]
fn linux_unshare_supported_uncached() -> bool {
    if let Ok(value) = std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone")
        && value.trim() == "0"
    {
        return false;
    }
    // /proc/self/ns/user existing is necessary for the syscall to do
    // anything useful; this also covers WSL1 (no namespaces).
    std::path::Path::new("/proc/self/ns/user").exists()
}

/// Stub for non-Linux compilation so the macOS / cross-compile builds keep
/// working without `#[cfg]` everywhere in callers.
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub(crate) fn linux_unshare_supported() -> bool {
    false
}

/// Seccomp deny-list applied to the shell child immediately before `exec`.
///
/// The Linux unshare + Landlock layers cover filesystem and (when network is
/// denied) outbound IP traffic, but they do not close pivot paths a malicious
/// command could use to reach back into the agent itself: `ptrace` against
/// sibling processes in the same user namespace, `process_vm_readv` /
/// `process_vm_writev` for cross-process memory, or `socket(AF_UNIX)` for
/// connecting to abstract or filesystem-backed local sockets the agent runs.
/// The sandbox returns `EPERM` for those calls; the kernel returns the
/// same errno a normal access check would, so a well-behaved tool
/// degrades to a clean error instead of crashing.
#[cfg(target_os = "linux")]
mod linux_seccomp {
    use std::collections::BTreeMap;

    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch, apply_filter,
    };

    /// Install the shell-child seccomp filter on the current thread.
    ///
    /// Must be called from inside the `pre_exec` hook after fork and before
    /// `execve` so only the child inherits it. Sets `PR_SET_NO_NEW_PRIVS`
    /// first because an unprivileged seccomp install requires NNP. Calling
    /// `prctl(PR_SET_NO_NEW_PRIVS)` twice is harmless if landlock has
    /// already enabled it on the same thread.
    pub(super) fn install_shell_filter() -> std::io::Result<()> {
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let program = build_shell_filter()?;
        apply_filter(&program).map_err(std::io::Error::other)?;
        Ok(())
    }

    /// Build the BPF program separately so tests can exercise the same
    /// rule set without going through a `pre_exec` fork dance.
    pub(super) fn build_shell_filter() -> std::io::Result<BpfProgram> {
        let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
        // Unconditional denies: empty rule vec matches every invocation.
        rules.insert(libc::SYS_ptrace, vec![]);
        rules.insert(libc::SYS_process_vm_readv, vec![]);
        rules.insert(libc::SYS_process_vm_writev, vec![]);

        // `socket(AF_UNIX, ...)` and `socketpair(AF_UNIX, ...)`: deny when
        // arg0 equals AF_UNIX. Other socket families fall through to the
        // default Allow action because IP-family sockets are governed by
        // CLONE_NEWNET (handled at unshare time) and ICMP/raw is governed
        // by capabilities the unprivileged shell does not hold.
        let unix_match = SeccompRule::new(vec![
            SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                libc::AF_UNIX as u64,
            )
            .map_err(std::io::Error::other)?,
        ])
        .map_err(std::io::Error::other)?;
        rules.insert(libc::SYS_socket, vec![unix_match.clone()]);
        rules.insert(libc::SYS_socketpair, vec![unix_match]);

        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(libc::EPERM as u32),
            target_arch(),
        )
        .map_err(std::io::Error::other)?;
        filter.try_into().map_err(std::io::Error::other)
    }

    /// Pick the seccomp `TargetArch` matching the build host. We only
    /// support the two architectures squeezy ships Linux binaries for;
    /// other targets fall through to a build-time `unimplemented!()` so a
    /// silent miscompilation cannot turn into a no-op filter.
    fn target_arch() -> TargetArch {
        #[cfg(target_arch = "x86_64")]
        {
            TargetArch::x86_64
        }
        #[cfg(target_arch = "aarch64")]
        {
            TargetArch::aarch64
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            unimplemented!("seccomp filter target arch is not supported");
        }
    }
}

#[cfg(test)]
#[path = "shell_sandbox_tests.rs"]
mod tests;
