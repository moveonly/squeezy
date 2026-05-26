//! Unit tests for the Linux seccomp shell-spawn deny-list.
//!
//! The tests fork the test process, install the filter on the child only,
//! and run a tiny probe that returns a sentinel exit code. This avoids
//! re-execing a helper binary while keeping the parent process unaffected
//! by the syscall restrictions.
//!
//! macOS and Windows builds compile this file as empty: there is no
//! seccomp surface to exercise on those platforms.

#![cfg(target_os = "linux")]

use seccompiler::apply_filter;

use super::linux_seccomp::build_shell_filter;

/// Spawn `child_fn` under the seccomp filter and return its exit code.
///
/// Cargo's test harness is multi-threaded, so the post-fork code path
/// must be async-signal-safe: we build the BPF program in the parent
/// (where the allocator is in a consistent state) and only call
/// `prctl`, `seccomp`, and a raw `write` to stderr in the child.
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

#[test]
fn build_shell_filter_succeeds_on_supported_arch() {
    // Sanity check: filter construction must succeed at build time even
    // before we attempt to install it on a thread.
    let _ = build_shell_filter().expect("build seccomp filter");
}

#[test]
fn filter_denies_ptrace_with_eperm() {
    assert_eq!(run_under_filter(probe_ptrace), 0);
}

#[test]
fn filter_denies_socket_af_unix_with_eperm() {
    assert_eq!(run_under_filter(probe_socket_af_unix), 0);
}

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
