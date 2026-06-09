//! Unit tests for the Linux seccomp shell-spawn deny-list and sandbox plan
//! metadata assertions.
//!
//! The seccomp tests fork the test process, install the filter on the child
//! only, and run a tiny probe that returns a sentinel exit code. This avoids
//! re-execing a helper binary while keeping the parent process unaffected by
//! the syscall restrictions.
//!
//! macOS and Windows builds compile only the cross-platform metadata tests;
//! the seccomp section is Linux-only.

use super::ShellSandboxPlan;

/// Verify that `exports_ask_socket` returns false for the
/// `linux-direct-syscalls` backend. The seccomp filter installed by that
/// backend denies `socket(AF_UNIX, …)`, so advertising `SQUEEZY_ASK_SOCKET`
/// to a child running under it would promise an unreachable capability.
#[test]
fn linux_direct_syscalls_does_not_export_ask_socket() {
    // Build a minimal plan that looks like the linux-direct-syscalls backend.
    let plan = ShellSandboxPlan {
        program: "sh".to_string(),
        args: vec!["-lc".to_string(), "echo hi".to_string()],
        backend: "linux-direct-syscalls",
        mode: "best_effort",
        network: "denied",
        filesystem: "enforced",
        required: false,
        configured_read_roots: vec![],
        configured_write_roots: vec![],
        filesystem_read_roots: vec![],
        filesystem_write_roots: vec![],
        fallback_reason: None,
        best_effort_fallback: None,
        selected_shell: None,
    };
    assert!(
        !plan.exports_ask_socket(),
        "linux-direct-syscalls must not export the ask socket (AF_UNIX denied by seccomp)"
    );
}

/// Verify that the `metadata()` payload exposes `ask_socket_suppressed = true`
/// and `landlock_active = true` for an enforced linux-direct-syscalls plan.
#[test]
fn linux_direct_syscalls_metadata_fields() {
    let plan = ShellSandboxPlan {
        program: "sh".to_string(),
        args: vec!["-lc".to_string(), "echo hi".to_string()],
        backend: "linux-direct-syscalls",
        mode: "required",
        network: "denied",
        filesystem: "enforced",
        required: true,
        configured_read_roots: vec![],
        configured_write_roots: vec![],
        filesystem_read_roots: vec![],
        filesystem_write_roots: vec![],
        fallback_reason: None,
        best_effort_fallback: None,
        selected_shell: None,
    };
    let meta = plan.metadata();
    assert_eq!(
        meta["ask_socket_suppressed"],
        serde_json::json!(true),
        "ask_socket_suppressed must be true for linux-direct-syscalls"
    );
    assert_eq!(
        meta["landlock_active"],
        serde_json::json!(true),
        "landlock_active must be true when filesystem == enforced on linux-direct-syscalls"
    );
}

/// Verify that the `metadata()` payload exposes `landlock_active = false`
/// when Landlock enforcement is unavailable (best_effort_unavailable).
#[test]
fn linux_direct_syscalls_metadata_landlock_inactive() {
    let plan = ShellSandboxPlan {
        program: "sh".to_string(),
        args: vec!["-lc".to_string(), "echo hi".to_string()],
        backend: "linux-direct-syscalls",
        mode: "best_effort",
        network: "denied",
        filesystem: "best_effort_unavailable",
        required: false,
        configured_read_roots: vec![],
        configured_write_roots: vec![],
        filesystem_read_roots: vec![],
        filesystem_write_roots: vec![],
        fallback_reason: None,
        best_effort_fallback: None,
        selected_shell: None,
    };
    let meta = plan.metadata();
    assert_eq!(
        meta["ask_socket_suppressed"],
        serde_json::json!(true),
        "ask_socket_suppressed must still be true when Landlock is unavailable"
    );
    assert_eq!(
        meta["landlock_active"],
        serde_json::json!(false),
        "landlock_active must be false when filesystem != enforced"
    );
}

/// Verify that non-Linux backends do not set ask_socket_suppressed incorrectly.
/// The `none` (direct, sandbox off) backend should export the ask socket.
#[test]
fn direct_backend_exports_ask_socket() {
    let plan = ShellSandboxPlan {
        program: "sh".to_string(),
        args: vec!["-lc".to_string(), "echo hi".to_string()],
        backend: "none",
        mode: "off",
        network: "not_enforced",
        filesystem: "not_enforced",
        required: false,
        configured_read_roots: vec![],
        configured_write_roots: vec![],
        filesystem_read_roots: vec![],
        filesystem_write_roots: vec![],
        fallback_reason: None,
        best_effort_fallback: None,
        selected_shell: None,
    };
    assert!(
        plan.exports_ask_socket(),
        "none backend must export the ask socket"
    );
    let meta = plan.metadata();
    assert_eq!(
        meta["ask_socket_suppressed"],
        serde_json::json!(false),
        "ask_socket_suppressed must be false for none backend"
    );
}

/// Spawn `child_fn` under the seccomp filter and return its exit code.
///
/// Cargo's test harness is multi-threaded, so the post-fork code path
/// must be async-signal-safe: we build the BPF program in the parent
/// (where the allocator is in a consistent state) and only call
/// `prctl`, `seccomp`, and a raw `write` to stderr in the child.
#[cfg(target_os = "linux")]
fn run_under_filter(child_fn: fn() -> i32) -> i32 {
    let program = build_shell_filter().expect("build seccomp filter");
    let pid = unsafe { libc::fork() };
    match pid.cmp(&0) {
        std::cmp::Ordering::Less => {
            panic!("fork failed: {}", std::io::Error::last_os_error())
        }
        std::cmp::Ordering::Equal => {
            // Child: install filter on this thread, run the probe, exit
            // with its return value.
            let nnp = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
            if nnp != 0 || apply_filter(&program).is_err() {
                let msg = b"install_shell_filter failed\n";
                unsafe {
                    libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
                    libc::_exit(127);
                }
            }
            let code = child_fn();
            unsafe { libc::_exit(code) };
        }
        std::cmp::Ordering::Greater => {
            // Parent: wait for the child and return its exit code or
            // 128+signo if it died from a signal.
            let mut status: libc::c_int = 0;
            if unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
                panic!("waitpid failed: {}", std::io::Error::last_os_error());
            }
            if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else if libc::WIFSIGNALED(status) {
                128 + libc::WTERMSIG(status)
            } else {
                -1
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn probe_ptrace() -> i32 {
    // PTRACE_TRACEME = 0; without seccomp this returns 0. With the
    // filter installed the kernel must surface EPERM.
    let result = unsafe { libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) };
    let errno = std::io::Error::last_os_error().raw_os_error();
    if result == 0 {
        10 // unexpected success
    } else if errno == Some(libc::EPERM) {
        0 // expected: filter denied
    } else {
        20 // wrong errno
    }
}

#[cfg(target_os = "linux")]
fn probe_socket_af_unix() -> i32 {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    let errno = std::io::Error::last_os_error().raw_os_error();
    if fd >= 0 {
        unsafe {
            libc::close(fd);
        }
        10
    } else if errno == Some(libc::EPERM) {
        0
    } else {
        20
    }
}

#[cfg(target_os = "linux")]
fn probe_socket_af_inet() -> i32 {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        // EPERM here would mean the filter denied AF_INET — which is
        // the regression we're guarding against. EACCES or similar
        // would indicate an unrelated host restriction (e.g.
        // CLONE_NEWNET active in the test runner).
        return std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    }
    unsafe {
        libc::close(fd);
    }
    0
}

#[cfg(target_os = "linux")]
#[test]
fn build_shell_filter_succeeds_on_supported_arch() {
    // Sanity check: filter construction must succeed at build time even
    // before we attempt to install it on a thread.
    let _ = build_shell_filter().expect("build seccomp filter");
}

#[cfg(target_os = "linux")]
#[test]
fn filter_denies_ptrace_with_eperm() {
    assert_eq!(run_under_filter(probe_ptrace), 0);
}

#[cfg(target_os = "linux")]
#[test]
fn filter_denies_socket_af_unix_with_eperm() {
    assert_eq!(run_under_filter(probe_socket_af_unix), 0);
}

#[cfg(target_os = "linux")]
#[test]
fn filter_allows_socket_af_inet() {
    // We only assert that AF_INET is NOT denied by EPERM. The socket
    // may still fail for unrelated reasons inside a network-namespaced
    // test runner (e.g. unsupported protocol); we explicitly tolerate
    // any non-EPERM errno.
    let errno = run_under_filter(probe_socket_af_inet);
    assert_ne!(
        errno,
        libc::EPERM,
        "AF_INET socket() should not be denied by seccomp filter"
    );
}

#[cfg(target_os = "linux")]
use seccompiler::apply_filter;

#[cfg(target_os = "linux")]
use super::linux_seccomp::build_shell_filter;
