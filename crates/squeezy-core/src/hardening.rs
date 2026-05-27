use crate::HardeningConfig;

pub fn pre_main_hardening(config: HardeningConfig) {
    #[cfg(target_os = "macos")]
    macos_hardening(config);
    #[cfg(not(target_os = "macos"))]
    {
        if config.disable_core_dumps {
            disable_core_dumps();
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_hardening(config: HardeningConfig) {
    if config.deny_debug_attach {
        unsafe {
            let _ = libc::ptrace(libc::PT_DENY_ATTACH, 0, std::ptr::null_mut(), 0);
        }
    }
    if config.disable_core_dumps {
        disable_core_dumps();
    }
    remove_env_vars_with_prefix("DYLD_");
    remove_env_vars_with_prefix("MallocStackLogging");
    remove_env_vars_with_prefix("MallocLogFile");
}

#[cfg(unix)]
fn disable_core_dumps() {
    let rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let _ = set_rlimit_or_warn("RLIMIT_CORE", libc::RLIMIT_CORE, &rlim);
}

#[cfg(not(unix))]
fn disable_core_dumps() {}

/// Apply `setrlimit(resource, rlim)` and surface failures via `tracing::warn!`
/// without aborting the process. Hardening is best-effort: a failure here
/// (e.g. `EPERM` under a wrapper that already lowered the hard limit) leaves
/// the prior limit in place rather than tearing down the user's session.
#[cfg(unix)]
fn set_rlimit_or_warn(
    name: &'static str,
    resource: SetrlimitResource,
    rlim: &libc::rlimit,
) -> Result<(), std::io::Error> {
    let ret = unsafe { libc::setrlimit(resource, rlim) };
    if ret == 0 {
        Ok(())
    } else {
        let err = std::io::Error::last_os_error();
        tracing::warn!(
            limit = name,
            errno = err.raw_os_error(),
            error = %err,
            "setrlimit failed; continuing without hardening this limit",
        );
        Err(err)
    }
}

// `setrlimit`'s resource argument is `__rlimit_resource_t` (u32) on glibc and
// `c_int` on musl/macOS; alias to whatever `libc::RLIMIT_CORE` resolves to so
// the helper compiles on every Unix target without per-target arms.
#[cfg(all(unix, target_env = "gnu"))]
type SetrlimitResource = libc::__rlimit_resource_t;
#[cfg(all(unix, not(target_env = "gnu")))]
type SetrlimitResource = libc::c_int;

#[cfg(target_os = "macos")]
fn remove_env_vars_with_prefix(prefix: &str) {
    let keys = std::env::vars_os()
        .filter_map(|(key, _)| key.into_string().ok())
        .filter(|key| key.starts_with(prefix))
        .collect::<Vec<_>>();
    for key in keys {
        unsafe {
            std::env::remove_var(key);
        }
    }
}

#[cfg(all(test, unix))]
#[path = "hardening_tests.rs"]
mod tests;
