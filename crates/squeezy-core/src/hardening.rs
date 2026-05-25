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
    unsafe {
        let _ = libc::setrlimit(libc::RLIMIT_CORE, &rlim);
    }
}

#[cfg(not(unix))]
fn disable_core_dumps() {}

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
