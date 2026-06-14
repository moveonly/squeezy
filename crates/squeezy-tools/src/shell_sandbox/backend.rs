use squeezy_core::ShellSandboxConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShellSandboxBackendMetadata {
    pub(crate) backend: &'static str,
    pub(crate) filesystem: &'static str,
    pub(crate) ask_socket_unavailable: Option<&'static str>,
    pub(crate) windows_posture: Option<&'static str>,
    pub(crate) windows_no_fs_sandbox: bool,
}

pub(crate) fn shell_sandbox_backend_metadata(
    config: &ShellSandboxConfig,
) -> ShellSandboxBackendMetadata {
    platform::metadata(config)
}

#[cfg(target_os = "linux")]
mod platform {
    use super::ShellSandboxBackendMetadata;
    use crate::shell_sandbox::{linux_landlock_supported, linux_unshare_supported};
    use squeezy_core::{ShellSandboxConfig, ShellSandboxMode};

    pub(super) fn metadata(config: &ShellSandboxConfig) -> ShellSandboxBackendMetadata {
        let will_use_direct_syscalls = linux_unshare_supported()
            && !matches!(
                config.mode,
                ShellSandboxMode::Off | ShellSandboxMode::External
            );
        if !will_use_direct_syscalls {
            return ShellSandboxBackendMetadata {
                backend: "none",
                filesystem: "not_enforced",
                ask_socket_unavailable: None,
                windows_posture: None,
                windows_no_fs_sandbox: false,
            };
        }
        ShellSandboxBackendMetadata {
            backend: "linux-direct-syscalls",
            filesystem: if linux_landlock_supported() {
                "enforced"
            } else {
                "best_effort_unavailable"
            },
            ask_socket_unavailable: Some(
                "squeezy ask is unavailable inside this shell child because the seccomp profile blocks AF_UNIX socket(2)",
            ),
            windows_posture: None,
            windows_no_fs_sandbox: false,
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::ShellSandboxBackendMetadata;
    use crate::shell_sandbox::macos_sandbox_exec_supported;
    use squeezy_core::{ShellSandboxConfig, ShellSandboxMode};

    pub(super) fn metadata(config: &ShellSandboxConfig) -> ShellSandboxBackendMetadata {
        if macos_sandbox_exec_supported()
            && !matches!(
                config.mode,
                ShellSandboxMode::Off | ShellSandboxMode::External
            )
        {
            ShellSandboxBackendMetadata {
                backend: "macos-sandbox-exec",
                filesystem: "enforced",
                ask_socket_unavailable: None,
                windows_posture: None,
                windows_no_fs_sandbox: false,
            }
        } else {
            ShellSandboxBackendMetadata {
                backend: "none",
                filesystem: "not_enforced",
                ask_socket_unavailable: None,
                windows_posture: None,
                windows_no_fs_sandbox: false,
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::ShellSandboxBackendMetadata;
    use crate::win_sandbox_spec;
    use squeezy_core::{ShellSandboxConfig, ShellSandboxMode, WindowsSandboxLevel};

    pub(super) fn metadata(config: &ShellSandboxConfig) -> ShellSandboxBackendMetadata {
        if matches!(
            config.mode,
            ShellSandboxMode::Off | ShellSandboxMode::External
        ) {
            return ShellSandboxBackendMetadata {
                backend: "none",
                filesystem: "not_enforced",
                ask_socket_unavailable: None,
                windows_posture: Some("job-object-only"),
                windows_no_fs_sandbox: true,
            };
        }
        match config.windows_sandbox_level {
            WindowsSandboxLevel::Disabled => ShellSandboxBackendMetadata {
                backend: "windows-job-object",
                filesystem: "best_effort_unavailable",
                ask_socket_unavailable: None,
                windows_posture: Some("job-object-only"),
                windows_no_fs_sandbox: true,
            },
            WindowsSandboxLevel::RestrictedToken => ShellSandboxBackendMetadata {
                backend: "windows-restricted-token",
                filesystem: "enforced_writes_only",
                ask_socket_unavailable: None,
                windows_posture: Some("restricted-token-writes-only"),
                windows_no_fs_sandbox: false,
            },
            WindowsSandboxLevel::Elevated
                if squeezy_win_sandbox::elevated_setup_is_complete(
                    &win_sandbox_spec::win_state_dir(),
                ) =>
            {
                ShellSandboxBackendMetadata {
                    backend: "windows-elevated",
                    filesystem: "enforced",
                    ask_socket_unavailable: None,
                    windows_posture: None,
                    windows_no_fs_sandbox: false,
                }
            }
            WindowsSandboxLevel::Elevated => ShellSandboxBackendMetadata {
                backend: "windows-restricted-token",
                filesystem: "enforced_writes_only",
                ask_socket_unavailable: None,
                windows_posture: Some("restricted-token-writes-only"),
                windows_no_fs_sandbox: false,
            },
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    use super::ShellSandboxBackendMetadata;
    use squeezy_core::ShellSandboxConfig;

    pub(super) fn metadata(_config: &ShellSandboxConfig) -> ShellSandboxBackendMetadata {
        ShellSandboxBackendMetadata {
            backend: "none",
            filesystem: "not_enforced",
            ask_socket_unavailable: None,
            windows_posture: None,
            windows_no_fs_sandbox: false,
        }
    }
}
