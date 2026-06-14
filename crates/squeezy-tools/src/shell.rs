use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    path::Path,
    process::Stdio,
    sync::{Arc, OnceLock, atomic::Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::fs;

#[cfg(unix)]
use std::os::fd::FromRawFd;

use serde::Deserialize;
use serde_json::{Value, json};
use squeezy_core::{
    PermissionCapability, PermissionRisk, Redactor, ShellSandboxConfig, ShellSandboxMode,
    sensitive_pattern_base,
};
use tokio::{
    process::Command,
    sync::{Mutex, OwnedMutexGuard, OwnedSemaphorePermit},
    time,
};
use tokio_util::sync::CancellationToken;

use crate::sha256_hex;
use crate::shell_ask_server::ShellAskServer;
use crate::shell_capture::{
    ShellStreamCapture, drain_or_abort, read_limited_pipe, split_shell_output,
};
use crate::shell_output::{insert_content_field, shape_shell_output};
use crate::shell_parse::{
    analyze_shell_command, dequote_token, expand_wrapper_segments, is_destructive_shell_segment,
    parse_shell_command, shell_coverage_warnings, shell_segments, tokenize_shell_segment,
};
use crate::shell_program::ShellProgram;
use crate::shell_sandbox::{
    ShellSandboxPlan, configure_linux_shell_sandbox, configure_shell_process_group,
    shell_sandbox_best_effort_fallback_reason, shell_sandbox_runtime_unavailable,
    shell_sandbox_status_metadata,
};
use crate::shell_spillover::{RawSidecar, ShellSpilloverInfo};
#[cfg(windows)]
use crate::win_job::ShellJob;
#[cfg(windows)]
use crate::win_sandbox_spec::build_win_spec;
use crate::{
    DEFAULT_SHELL_OUTPUT_BYTE_CAP, DEFAULT_SHELL_TIMEOUT_MS, IO_DRAIN_TIMEOUT_MS,
    MAX_SHELL_OUTPUT_BYTE_CAP, MAX_SHELL_TIMEOUT_MS, OutputMode, SQUEEZY_ASK_CALL_ID_ENV,
    SQUEEZY_ASK_SOCKET_ENV, ShellAskApprover, ShellPermissionAnalysis, ToolCall, ToolCostHint,
    ToolRegistry, ToolResult, ToolStatus, make_result, shell_exit_signal, tool_arg_error,
    tool_error,
};

pub(crate) struct ShellRunOutcome {
    pub(crate) exit_status: Option<std::process::ExitStatus>,
    pub(crate) timed_out: bool,
    pub(crate) stdout_bytes: Vec<u8>,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_bytes: Vec<u8>,
    pub(crate) stderr_truncated: bool,
    /// Full pre-cap, redacted output streamed to a `{call_id}-raw.txt`
    /// sidecar when the hard byte cap dropped bytes the in-memory capture
    /// could not keep. `None` when output stayed under the cap.
    pub(crate) raw_spillover: Option<ShellSpilloverInfo>,
    /// Windows-only: status of the Win32 Job Object created for
    /// process-tree cleanup on the non-sandboxed path. Surfaced in audit
    /// rows so process-tree containment is independently observable even
    /// when FS/network isolation is unavailable. `None` on all other
    /// platforms.
    pub(crate) win_job_object_status: Option<&'static str>,
    /// True when `tty = true` was requested but the platform degraded to
    /// non-TTY pipes (Windows without ConPTY). The caller surfaces this as a
    /// note in the tool result.
    pub(crate) tty_degraded: bool,
    /// Set when `tty: true` was requested but the platform does not support
    /// ConPTY and the run fell back to piped stdin/stdout/stderr. The model
    /// should be informed so it can adjust output-parsing expectations.
    pub(crate) tty_downgraded: bool,
    /// Windows Job Object assignment status. `None` means no Job Object was
    /// attempted (Unix, or a Windows sandbox tier that manages its own job).
    /// `Some(true)` means the process was successfully assigned; `Some(false)`
    /// means creation or assignment failed and process-tree cleanup is
    /// best-effort only.
    pub(crate) windows_job_assigned: Option<bool>,
    /// Result of `TerminateJobObject` called during a timeout cleanup.
    /// `None` when not on Windows, not timed out, or no job object was held.
    /// `Some(false)` means descendants may survive the timeout.
    pub(crate) windows_timeout_job_cleanup_ok: Option<bool>,
    /// Present when the child was killed via Unix signals (timeout or
    /// cancellation). Records pgid targeted, whether each signal syscall
    /// succeeded, whether the grace period expired, and whether a
    /// direct-child fallback kill was also issued.
    pub(crate) kill_meta: Option<ShellKillMeta>,
    /// Set when `ShellAskServer::start` failed even though the sandbox backend
    /// supports AF_UNIX ask sockets.  The string is a human-readable reason
    /// suitable for the `nested_ask.reason` field in the JSON result.
    pub(crate) ask_server_start_error: Option<String>,
}

/// Termination mechanics recorded when a Unix shell child was killed by
/// signal (on timeout or cancellation). Surfaces process-group signal
/// outcomes so that Linux users can see whether pgid-based cleanup
/// succeeded and whether detached descendants may have survived.
pub(crate) struct ShellKillMeta {
    /// Process group id we *targeted* with `kill(-pgid, …)`. This is the
    /// child's pid, since `configure_shell_process_group` arranges a child-side
    /// `setpgid(0, 0)` at exec time so the freshly-spawned process is its own
    /// pgid leader. `pgid` may not match `getpgid(child)` if the child later
    /// re-set its process group via `setpgid(0, X)` with `X != pid`; in that
    /// case `sigterm_ok` / `sigkill_ok` will surface the kernel's `ESRCH`
    /// response (the targeted pgid no longer points at the child).
    pub(crate) pgid: u32,
    /// Whether `kill(-pgid, SIGTERM)` returned without error.
    pub(crate) sigterm_ok: bool,
    /// Whether the grace period expired before the child exited
    /// (i.e. SIGKILL escalation was required).
    pub(crate) grace_expired: bool,
    /// Whether `kill(-pgid, SIGKILL)` was sent.
    pub(crate) sigkill_sent: bool,
    /// Whether `kill(-pgid, SIGKILL)` returned without error.
    pub(crate) sigkill_ok: bool,
    /// Whether `child.kill()` was also called as a direct-child fallback.
    /// This is `true` only when SIGKILL escalation was required (grace period
    /// expired) and `child.kill()` was issued as a supplemental direct-child
    /// signal after the process-group SIGKILL. It is `false` when the child
    /// exited during the SIGTERM grace period, in which case no SIGKILL or
    /// direct-child kill was needed.
    pub(crate) direct_child_fallback: bool,
    /// Whether the supplemental `child.kill()` direct-child fallback returned
    /// `Ok(())`. Only meaningful when [`direct_child_fallback`] is `true`. A
    /// `false` value here usually means the kernel had already reaped the
    /// direct child via the prior pgid `SIGKILL`, in which case tokio reports
    /// `InvalidInput`. The companion field exists so a troubleshooter can tell
    /// whether *any* of the three signal attempts (pgid SIGTERM, pgid SIGKILL,
    /// direct-child kill) reached a live process.
    pub(crate) direct_child_kill_ok: bool,
}

struct ShellRunRequest<'a> {
    sandbox_plan: &'a ShellSandboxPlan,
    workdir: &'a Path,
    timeout_ms: u64,
    output_cap: usize,
    tty: bool,
    cancel: &'a CancellationToken,
    call: &'a ToolCall,
    command_text: &'a str,
    shell_ask_approver: Option<ShellAskApprover>,
}

pub(crate) struct ShellExecutionGuard {
    pub(crate) _permit: OwnedSemaphorePermit,
    pub(crate) _workdir: OwnedMutexGuard<()>,
}

enum ShellRunError {
    /// Shell was cancelled. Carries Unix process-group kill metadata and the
    /// Windows Job Object cleanup result (`None` = no job object was active;
    /// `Some(true)` = job terminated successfully; `Some(false)` = terminate
    /// failed and descendants may survive) so callers can include both in audit
    /// and result payloads.
    Cancelled {
        kill_meta: Option<ShellKillMeta>,
        windows_job_cleanup_ok: Option<bool>,
    },
    SandboxStartDenied(String),
    /// Spawn failure on a non-required sandboxed backend where the OS or
    /// container environment blocked the spawn/pre-exec path (e.g. the
    /// Linux `pre_exec` hook was denied by the parent's seccomp policy).
    /// Unlike `Io`, this is treated as a best_effort degradation — the
    /// caller records the failure and retries on the unsandboxed direct
    /// path instead of surfacing a hard tool error.
    SpawnFallback(String),
    Io(std::io::Error),
}

/// Decide whether to call `TerminateJobObject` and report the resulting
/// cleanup state. Shared by the cancel and timeout cleanup paths because
/// both observe the same three states:
///
/// - `assigned == Some(true)`: the process is in the job, so termination
///   should propagate to the full process tree; report the terminate
///   result.
/// - `assigned == Some(false)`: the job was created but the process never
///   landed in it, so `TerminateJobObject` on an empty job would silently
///   return `Ok` while descendants survive — report `Some(false)` and skip
///   the call so callers can surface the leak.
/// - `assigned == None`: the sandbox tier owns its own job (or this is a
///   non-Windows path), so there is nothing to clean up at this layer.
#[cfg(windows)]
fn windows_job_cleanup_status(job: Option<&ShellJob>, assigned: Option<bool>) -> Option<bool> {
    match assigned {
        Some(true) => job.map(|j| j.terminate(1).is_ok()),
        Some(false) => Some(false),
        None => None,
    }
}

impl ToolRegistry {
    pub(crate) async fn execute_shell(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
        group_id: &str,
        shell_ask_approver: Option<ShellAskApprover>,
    ) -> ToolResult {
        self.execute_shell_capped(
            call,
            cancel,
            MAX_SHELL_TIMEOUT_MS,
            group_id,
            shell_ask_approver,
        )
        .await
    }

    pub(crate) async fn execute_shell_capped(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
        max_timeout_ms: u64,
        group_id: &str,
        shell_ask_approver: Option<ShellAskApprover>,
    ) -> ToolResult {
        let args = match serde_json::from_value::<ShellArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let analysis = analyze_shell_command(&args.command);
        if args.command.trim().is_empty() {
            return shell_policy_denied(call, &analysis, "shell command must not be empty");
        }
        if args.timeout_ms == Some(0) {
            return shell_policy_denied(call, &analysis, "shell timeout_ms must be at least 1");
        }
        if args.output_byte_cap == Some(0) {
            return shell_policy_denied(
                call,
                &analysis,
                "shell output_byte_cap must be at least 1",
            );
        }
        // The fast path that skips sandboxing and checkpointing is gated by
        // BOTH a `local-shell-` call_id prefix AND a per-process nonce that
        // only the TUI's `!cmd` minter holds. Stripping either side leaves the
        // command on the normal model-tool path with full sandbox/checkpoint
        // guarantees. See [`direct_user_shell_nonce`] for why this combination
        // is unforgeable from outside the process.
        let nonce_ok = args
            .direct_user_shell_nonce
            .as_deref()
            .is_some_and(|nonce| {
                constant_time_eq(nonce.as_bytes(), direct_user_shell_nonce().as_bytes())
            });
        let direct_user_shell =
            args.direct_user_shell && call.call_id.starts_with("local-shell-") && nonce_ok;
        let workdir = match self.resolve_shell_workdir(args.workdir.as_deref().unwrap_or(".")) {
            Ok(path) => path,
            Err(err) => {
                return shell_policy_denied(
                    call,
                    &analysis,
                    format!("shell workdir rejected by cwd policy: {err}"),
                );
            }
        };
        let implicit_skill = self
            .skills_snapshot()
            .detect_for_command(&args.command, &workdir);
        let _shell_guard = match self.shell_execution_guard(&workdir).await {
            Ok(guard) => guard,
            Err(err) => return tool_error(call, err),
        };
        let timeout_ms = args
            .timeout_ms
            .unwrap_or(DEFAULT_SHELL_TIMEOUT_MS)
            .min(max_timeout_ms);
        let output_cap = args
            .output_byte_cap
            .unwrap_or(DEFAULT_SHELL_OUTPUT_BYTE_CAP)
            .min(MAX_SHELL_OUTPUT_BYTE_CAP);
        let checkpoint_before = if shell_command_needs_checkpoint(direct_user_shell, &analysis)
            && self.has_checkpoint_provider()
        {
            match self.track_checkpoint_tree() {
                Ok(snapshot) => snapshot,
                Err(err) => return tool_error(call, err),
            }
        } else {
            None
        };
        if let Some(pattern) = shell_command_references_sensitive_path(
            &args.command,
            &self.shell_sandbox.sensitive_path_patterns,
        ) {
            let reason = format!("shell command references sensitive path pattern {pattern:?}");
            self.audit_shell(
                call,
                &args,
                &workdir,
                &analysis,
                shell_sandbox_status_metadata(&self.shell_sandbox, "denied"),
                timeout_ms,
                output_cap,
                "denied",
                Some(&reason),
                None,
                &[],
                &[],
            );
            return shell_policy_denied(call, &analysis, reason);
        }
        if let Some(name) = shell_command_writes_protected_metadata(
            &args.command,
            &self.shell_sandbox.protected_metadata_names,
        ) {
            let reason = format!("shell command writes protected metadata directory {name:?}");
            self.audit_shell(
                call,
                &args,
                &workdir,
                &analysis,
                shell_sandbox_status_metadata(&self.shell_sandbox, "denied"),
                timeout_ms,
                output_cap,
                "denied",
                Some(&reason),
                None,
                &[],
                &[],
            );
            return shell_policy_denied(call, &analysis, reason);
        }

        // Validate the SQUEEZY_SHELL override before constructing any plan so
        // that a missing Git Bash (or similar unresolvable shell) produces a
        // clear tool error even for the direct_user_shell TUI fast path.
        if let Err(reason) = ShellProgram::validate_override() {
            self.audit_shell(
                call,
                &args,
                &workdir,
                &analysis,
                shell_sandbox_status_metadata(&self.shell_sandbox, "denied"),
                timeout_ms,
                output_cap,
                "denied",
                Some(&reason),
                None,
                &[],
                &[],
            );
            return shell_policy_denied(call, &analysis, reason);
        }

        let mut sandbox_plan = if direct_user_shell {
            ShellSandboxPlan::direct(&args.command, ShellSandboxMode::Off, &self.shell_sandbox)
        } else {
            match self.prepare_shell_sandbox(&args.command, &analysis).await {
                Ok(plan) => plan,
                Err(reason) => {
                    self.audit_shell(
                        call,
                        &args,
                        &workdir,
                        &analysis,
                        shell_sandbox_status_metadata(&self.shell_sandbox, "unavailable"),
                        timeout_ms,
                        output_cap,
                        "denied",
                        Some(&reason),
                        None,
                        &[],
                        &[],
                    );
                    return shell_policy_denied(call, &analysis, reason);
                }
            }
        };

        let mut run = match self
            .run_shell_plan(ShellRunRequest {
                sandbox_plan: &sandbox_plan,
                workdir: &workdir,
                timeout_ms,
                output_cap,
                tty: args.tty,
                cancel: &cancel,
                call,
                command_text: &args.command,
                shell_ask_approver: shell_ask_approver.clone(),
            })
            .await
        {
            Ok(run) => run,
            Err(ShellRunError::Cancelled {
                kill_meta,
                windows_job_cleanup_ok,
            }) => {
                let error_msg = shell_cancelled_error_msg(kill_meta.as_ref());
                self.audit_shell(
                    call,
                    &args,
                    &workdir,
                    &analysis,
                    sandbox_plan.metadata(),
                    timeout_ms,
                    output_cap,
                    "cancelled",
                    Some(&error_msg),
                    None,
                    &[],
                    &[],
                );
                return shell_cancelled_result(call, windows_job_cleanup_ok, kill_meta);
            }
            Err(ShellRunError::SandboxStartDenied(reason)) => {
                self.audit_shell(
                    call,
                    &args,
                    &workdir,
                    &analysis,
                    sandbox_plan.metadata(),
                    timeout_ms,
                    output_cap,
                    "denied",
                    Some(&reason),
                    None,
                    &[],
                    &[],
                );
                return shell_policy_denied(call, &analysis, reason);
            }
            Err(ShellRunError::SpawnFallback(reason)) => {
                // The sandbox spawn/pre-exec was blocked by the host environment
                // (e.g. the container's seccomp policy denied unshare in the
                // Linux pre_exec hook). Treat as a best_effort degradation:
                // record the failure, mark the backend unavailable, and retry
                // on the unsandboxed direct path below.
                let degraded_backend = sandbox_plan.backend;
                let record = self.shell_sandbox_health.record_best_effort_fallback();
                self.audit_shell(
                    call,
                    &args,
                    &workdir,
                    &analysis,
                    sandbox_plan.metadata_with_best_effort_fallback(
                        degraded_backend,
                        &record,
                        Some(&reason),
                    ),
                    timeout_ms,
                    output_cap,
                    "fallback",
                    Some(&reason),
                    None,
                    &[],
                    &[],
                );
                self.shell_sandbox_health
                    .mark_unavailable(sandbox_plan.backend, reason.clone());
                sandbox_plan = ShellSandboxPlan::direct_with_fallback_record(
                    &args.command,
                    self.shell_sandbox.mode,
                    &self.shell_sandbox,
                    Some(reason),
                    Some((degraded_backend, record)),
                );
                match self
                    .run_shell_plan(ShellRunRequest {
                        sandbox_plan: &sandbox_plan,
                        workdir: &workdir,
                        timeout_ms,
                        output_cap,
                        tty: args.tty,
                        cancel: &cancel,
                        call,
                        command_text: &args.command,
                        shell_ask_approver: shell_ask_approver.clone(),
                    })
                    .await
                {
                    Ok(run) => run,
                    Err(ShellRunError::Cancelled {
                        kill_meta,
                        windows_job_cleanup_ok,
                    }) => {
                        let error_msg = shell_cancelled_error_msg(kill_meta.as_ref());
                        self.audit_shell(
                            call,
                            &args,
                            &workdir,
                            &analysis,
                            sandbox_plan.metadata(),
                            timeout_ms,
                            output_cap,
                            "cancelled",
                            Some(&error_msg),
                            None,
                            &[],
                            &[],
                        );
                        return shell_cancelled_result(call, windows_job_cleanup_ok, kill_meta);
                    }
                    Err(ShellRunError::SandboxStartDenied(reason)) => {
                        self.audit_shell(
                            call,
                            &args,
                            &workdir,
                            &analysis,
                            sandbox_plan.metadata(),
                            timeout_ms,
                            output_cap,
                            "denied",
                            Some(&reason),
                            None,
                            &[],
                            &[],
                        );
                        return shell_policy_denied(call, &analysis, reason);
                    }
                    Err(ShellRunError::SpawnFallback(reason)) => {
                        return tool_error(call, std::io::Error::other(reason));
                    }
                    Err(ShellRunError::Io(err)) => return tool_error(call, err),
                }
            }
            Err(ShellRunError::Io(err)) => return tool_error(call, err),
        };
        if let Some(reason) = shell_sandbox_best_effort_fallback_reason(&sandbox_plan, &run) {
            let exit_code = run.exit_status.as_ref().and_then(|status| status.code());
            // Record the fallback BEFORE the audit row + the retry plan so
            // the JSON metadata embedded in both already carries the
            // counter and one-shot latch the agent layer pivots on for
            // telemetry + a one-shot TUI warning.
            let degraded_backend = sandbox_plan.backend;
            let record = self.shell_sandbox_health.record_best_effort_fallback();
            self.audit_shell(
                call,
                &args,
                &workdir,
                &analysis,
                sandbox_plan.metadata_with_best_effort_fallback(
                    degraded_backend,
                    &record,
                    Some(&reason),
                ),
                timeout_ms,
                output_cap,
                "fallback",
                Some(&reason),
                exit_code,
                &run.stdout_bytes,
                &run.stderr_bytes,
            );
            self.shell_sandbox_health
                .mark_unavailable(sandbox_plan.backend, reason.clone());
            sandbox_plan = ShellSandboxPlan::direct_with_fallback_record(
                &args.command,
                self.shell_sandbox.mode,
                &self.shell_sandbox,
                Some(reason),
                Some((degraded_backend, record)),
            );
            run = match self
                .run_shell_plan(ShellRunRequest {
                    sandbox_plan: &sandbox_plan,
                    workdir: &workdir,
                    timeout_ms,
                    output_cap,
                    tty: args.tty,
                    cancel: &cancel,
                    call,
                    command_text: &args.command,
                    shell_ask_approver: shell_ask_approver.clone(),
                })
                .await
            {
                Ok(run) => run,
                Err(ShellRunError::Cancelled {
                    kill_meta,
                    windows_job_cleanup_ok,
                }) => {
                    let error_msg = shell_cancelled_error_msg(kill_meta.as_ref());
                    self.audit_shell(
                        call,
                        &args,
                        &workdir,
                        &analysis,
                        sandbox_plan.metadata(),
                        timeout_ms,
                        output_cap,
                        "cancelled",
                        Some(&error_msg),
                        None,
                        &[],
                        &[],
                    );
                    return shell_cancelled_result(call, windows_job_cleanup_ok, kill_meta);
                }
                Err(ShellRunError::SandboxStartDenied(reason)) => {
                    self.audit_shell(
                        call,
                        &args,
                        &workdir,
                        &analysis,
                        sandbox_plan.metadata(),
                        timeout_ms,
                        output_cap,
                        "denied",
                        Some(&reason),
                        None,
                        &[],
                        &[],
                    );
                    return shell_policy_denied(call, &analysis, reason);
                }
                Err(ShellRunError::SpawnFallback(reason)) => {
                    return tool_error(call, std::io::Error::other(reason));
                }
                Err(ShellRunError::Io(err)) => return tool_error(call, err),
            };
        }

        let ShellRunOutcome {
            exit_status,
            timed_out,
            stdout_bytes,
            stdout_truncated,
            stderr_bytes,
            stderr_truncated,
            raw_spillover,
            win_job_object_status,
            tty_degraded,
            tty_downgraded,
            windows_job_assigned,
            windows_timeout_job_cleanup_ok,
            kill_meta,
            ask_server_start_error,
        } = run;

        let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
        let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
        let redacted_stdout = self.redactor.redact(&stdout);
        let redacted_stderr = self.redactor.redact(&stderr);
        let stdout_redactions = redacted_stdout.redactions;
        let stderr_redactions = redacted_stderr.redactions;
        // On Windows, TTY requests degrade to non-TTY pipes because ConPTY is
        // not yet wired up. Prepend a brief notice so the model and user are
        // aware that interactive prompts may not work as expected (Bug 7).
        #[cfg(not(unix))]
        let stdout = if args.tty {
            let mut prefixed =
                String::from("[ConPTY unavailable; running non-interactive pipes]\n");
            prefixed.push_str(&redacted_stdout.text);
            prefixed
        } else {
            redacted_stdout.text
        };
        #[cfg(unix)]
        let stdout = redacted_stdout.text;
        let stderr = redacted_stderr.text;
        let truncated = stdout_truncated || stderr_truncated || timed_out;
        // Preserve the redacted bytes the agent would otherwise lose to
        // middle-truncation by writing them to a per-session tempfile.
        // Failures here are non-fatal (budget exhausted, temp disk full,
        // etc.); the shell tool still returns the truncated body so the
        // model can make a decision with the bytes it has.
        let spillover = if truncated {
            self.shell_spillover
                .spill(&call.call_id, stdout.as_bytes(), stderr.as_bytes())
        } else {
            None
        };
        let cost = ToolCostHint {
            output_bytes: (stdout.len() + stderr.len()) as u64,
            redactions: stdout_redactions + stderr_redactions,
            truncated,
            ..ToolCostHint::default()
        };
        let exit_code = exit_status.as_ref().and_then(|status| status.code());
        let exit_signal = shell_exit_signal(exit_status.as_ref());
        // Augment the sandbox metadata with the Windows Job Object outcome so
        // the audit row independently shows whether process-tree cleanup is
        // active, even when filesystem/network isolation is unavailable.
        let sandbox_metadata = {
            let mut meta = sandbox_plan.metadata();
            if let Some(status) = win_job_object_status
                && let Some(obj) = meta.as_object_mut()
            {
                obj.insert(
                    "win_job_object_status".to_string(),
                    Value::String(status.to_string()),
                );
            }
            meta
        };
        if sandbox_plan.required
            && shell_sandbox_runtime_unavailable(&sandbox_plan, exit_code, &stderr)
        {
            let reason = format!(
                "required shell sandbox backend {} failed at runtime",
                sandbox_plan.backend
            );
            self.shell_sandbox_health
                .mark_unavailable(sandbox_plan.backend, reason.clone());
            self.audit_shell(
                call,
                &args,
                &workdir,
                &analysis,
                sandbox_metadata,
                timeout_ms,
                output_cap,
                "denied",
                Some(&reason),
                exit_code,
                &stdout_bytes,
                &stderr_bytes,
            );
            return shell_policy_denied(call, &analysis, reason);
        }
        let status = if exit_status.as_ref().is_some_and(|status| status.success()) {
            ToolStatus::Success
        } else {
            ToolStatus::Error
        };
        let termination = shell_termination_reason(timed_out, timeout_ms, exit_code, exit_signal);
        let error = termination.clone();
        self.audit_shell(
            call,
            &args,
            &workdir,
            &analysis,
            sandbox_metadata.clone(),
            timeout_ms,
            output_cap,
            if timed_out {
                "timeout"
            } else if status == ToolStatus::Success {
                "success"
            } else {
                "error"
            },
            error.as_deref(),
            exit_code,
            &stdout_bytes,
            &stderr_bytes,
        );
        self.invalidate_diff_cache();

        // The `command_wrapper` shows the actual process spawned (e.g.
        // `sh -lc` on Linux/macOS) so POSIX-shell vs Bash behavior is explicit.
        // For the macOS `sandbox-exec` backend the `-p <profile>` argument
        // contains a multi-KB policy string; we summarize it as `<sandbox-profile>`
        // so the field stays concise and readable.
        let command_wrapper = {
            // Collect args up to (but not including) the user command literal,
            // which is always the last element on Unix shell plans.
            let display_args: Vec<String> = sandbox_plan.args
                [..sandbox_plan.args.len().saturating_sub(1)]
                .iter()
                .enumerate()
                .map(|(i, arg)| {
                    // When the previous arg was `-p`, this arg is the sandbox-exec
                    // profile text which can be many KB; replace it with a placeholder.
                    let prev = if i > 0 {
                        sandbox_plan.args.get(i - 1).map(|s| s.as_str())
                    } else {
                        None
                    };
                    if prev == Some("-p") {
                        "<sandbox-profile>".to_string()
                    } else {
                        arg.clone()
                    }
                })
                .collect();
            if display_args.is_empty() {
                sandbox_plan.program.clone()
            } else {
                format!("{} {}", sandbox_plan.program, display_args.join(" "))
            }
        };
        let mut raw_content = json!({
            "command": args.command,
            "workdir": self.relative(&workdir).to_string_lossy(),
            "command_wrapper": command_wrapper,
            "exit_code": exit_code,
            "signal": exit_signal,
            "termination": termination,
            "stdout": stdout,
            "stderr": stderr,
            "error": error,
            "truncated": truncated,
        });
        // Always surface sandbox status fields so the model can detect
        // Linux-specific constraints (ask socket suppressed, Landlock active)
        // without parsing the backend string.
        {
            let mut sandbox_info = serde_json::Map::new();
            sandbox_info.insert("backend".to_string(), sandbox_metadata["backend"].clone());
            sandbox_info.insert("network".to_string(), sandbox_metadata["network"].clone());
            sandbox_info.insert(
                "filesystem".to_string(),
                sandbox_metadata["filesystem"].clone(),
            );
            sandbox_info.insert(
                "ask_socket_suppressed".to_string(),
                sandbox_metadata["ask_socket_suppressed"].clone(),
            );
            sandbox_info.insert(
                "landlock_active".to_string(),
                sandbox_metadata["landlock_active"].clone(),
            );
            if let Some(fallback) = sandbox_metadata.get("best_effort_fallback").cloned() {
                sandbox_info.insert("best_effort_fallback".to_string(), fallback);
            }
            insert_content_field(&mut raw_content, "sandbox", Value::Object(sandbox_info));
        }
        // Expose Windows Job Object process-tree cleanup status so the audit
        // and the model can distinguish genuine cleanup from a degraded state.
        // "assigned" = process covered; "not_assigned" = assignment failed
        // (process may have raced to exit before assignment); "creation_failed"
        // = Job Object could not be created at all.
        if let Some(job_status) = win_job_object_status {
            insert_content_field(
                &mut raw_content,
                "windows_process_tree",
                json!({ "job_object": job_status }),
            );
        }
        // Surface a note when tty=true was requested but the platform degraded
        // to non-TTY pipe-backed stdio (Windows without ConPTY wired up).
        if tty_degraded {
            insert_content_field(
                &mut raw_content,
                "tty_note",
                json!(
                    "tty=true requested; ConPTY unavailable on this platform; ran with pipe-backed stdio"
                ),
            );
        }
        // Windows: surface the degraded sandbox state and Job Object assignment
        // so the agent layer can emit the once-per-session TUI banner and
        // callers can detect missing process-tree cleanup guarantees.
        if sandbox_plan.backend == "windows-job-object"
            || sandbox_plan.filesystem == "best_effort_unavailable"
        {
            // The Unix `best_effort_fallback` writer above and this Windows
            // writer both target the same `sandbox` key, and
            // `insert_content_field` overwrites rather than merges. The two
            // paths are mutually exclusive today (a best-effort fallback
            // rewrites the plan to `backend: "none"`, `filesystem:
            // "not_enforced"`, neither of which satisfies the predicate
            // above), so this assertion locks the invariant in debug builds
            // — any future plan-shape change that lets both fire would
            // otherwise silently drop the Unix payload.
            debug_assert!(
                raw_content
                    .as_object()
                    .and_then(|obj| obj.get("sandbox"))
                    .and_then(|s| s.as_object())
                    .map(|s| !s.contains_key("best_effort_fallback"))
                    .unwrap_or(true),
                "sandbox.best_effort_fallback and sandbox.windows_degraded must be mutually exclusive",
            );
            let first_in_session = self.shell_sandbox_health.record_windows_degraded();
            // Include the resolved shell so approval prompts and model context
            // show which shell interpreted the command.
            let resolved_shell = std::path::Path::new(&sandbox_plan.program)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&sandbox_plan.program)
                .to_string();
            insert_content_field(
                &mut raw_content,
                "sandbox",
                json!({
                    "windows_degraded": {
                        "first_in_session": first_in_session,
                        "backend": sandbox_plan.backend,
                        "filesystem": sandbox_plan.filesystem,
                        "resolved_shell": resolved_shell,
                    }
                }),
            );
        }
        if windows_job_assigned.is_some() || windows_timeout_job_cleanup_ok.is_some() {
            let mut windows_info = serde_json::Map::new();
            if let Some(assigned) = windows_job_assigned {
                windows_info.insert("job_object_assigned".to_string(), json!(assigned));
                windows_info.insert(
                    "cleanup_note".to_string(),
                    json!(if assigned {
                        "process tree will be terminated with the job"
                    } else {
                        "Job Object assignment failed; only direct child is covered by kill_on_drop"
                    }),
                );
            }
            if let Some(cleanup_ok) = windows_timeout_job_cleanup_ok {
                windows_info.insert("timeout_job_cleanup".to_string(), json!(cleanup_ok));
                if !cleanup_ok {
                    windows_info.insert(
                        "timeout_cleanup_note".to_string(),
                        json!("TerminateJobObject failed on timeout; descendant processes may still be running"),
                    );
                }
            }
            insert_content_field(
                &mut raw_content,
                "windows",
                serde_json::Value::Object(windows_info),
            );
        }
        if tty_downgraded {
            insert_content_field(
                &mut raw_content,
                "tty_downgraded",
                json!("TTY requested but ConPTY is not available; ran with pipes"),
            );
        }
        // Always inform the model and user about whether nested `squeezy ask`
        // approvals are usable for this shell call. Emitting the field
        // unconditionally with an explicit `available: true|false` removes the
        // earlier "absence implies available" ambiguity, which masked at least
        // three real cases: (a) sandbox policy forbids AF_UNIX sockets (e.g.
        // linux-direct-syscalls), (b) ShellAskServer::start failed at runtime
        // even though the backend supports AF_UNIX sockets, and (c) the caller
        // did not pass a `shell_ask_approver`, so no ask socket would have been
        // exported even if the sandbox allowed it. Without this field the
        // child shell sees a missing SQUEEZY_ASK_SOCKET env var, which looks
        // like a misconfiguration rather than an intentional policy.
        let nested_ask_block = if let Some(reason) = sandbox_plan
            .nested_ask_disabled_reason()
            .or(ask_server_start_error)
        {
            json!({ "available": false, "reason": reason })
        } else if shell_ask_approver.is_some() {
            json!({ "available": true })
        } else {
            json!({
                "available": false,
                "reason": "nested ask disabled: no approver wired to this shell call",
            })
        };
        insert_content_field(&mut raw_content, "nested_ask", nested_ask_block);
        // When the child was killed via Unix signals, surface the kill
        // mechanics so users can see whether pgid-based cleanup succeeded
        // and whether detached descendants (setsid / daemonized) may survive.
        if let Some(km) = kill_meta {
            insert_content_field(
                &mut raw_content,
                "kill_meta",
                json!({
                    "pgid": km.pgid,
                    "sigterm_ok": km.sigterm_ok,
                    "grace_expired": km.grace_expired,
                    "sigkill_sent": km.sigkill_sent,
                    "sigkill_ok": km.sigkill_ok,
                    "direct_child_fallback": km.direct_child_fallback,
                    "direct_child_kill_ok": km.direct_child_kill_ok,
                    "caveat_text": "process-group signals cannot reach setsid/daemonized descendants; detached processes may survive until cgroup-backed cleanup is available",
                }),
            );
        }
        if let Some(summary) = implicit_skill {
            insert_content_field(
                &mut raw_content,
                "implicit_skill_activation",
                json!({
                    "name": summary.name,
                    "source": "implicit",
                    "skill_source": summary.source,
                    "location": summary.location,
                }),
            );
        }
        if let Some(spill) = spillover.as_ref() {
            insert_content_field(
                &mut raw_content,
                "spillover",
                shell_spillover_metadata(spill),
            );
        }
        // The capped `spillover` above mirrors only the bytes that survived
        // the in-memory hard cap; `raw_spillover` carries the *full* pre-cap
        // output streamed straight from the pipe, so a long build log or
        // stack trace can be recovered in its entirety. Present only when the
        // cap actually dropped bytes.
        if let Some(raw_spill) = raw_spillover.as_ref() {
            insert_content_field(
                &mut raw_content,
                "raw_spillover",
                shell_spillover_metadata(raw_spill),
            );
        }
        if let Some(checkpoint_before) = checkpoint_before.as_ref() {
            let coverage_warnings = shell_coverage_warnings(&args.command);
            self.append_checkpoint_to_content(
                &mut raw_content,
                Some(checkpoint_before),
                call,
                group_id,
                status,
                coverage_warnings,
            );
        }
        let raw_result = make_result(call, status, raw_content.clone(), cost.clone(), None);
        let raw_output = raw_result.model_output();
        let raw_output_sha256 = raw_result.receipt.output_sha256.clone();
        if !args.output_mode.unwrap_or_default().is_shaped() {
            return raw_result;
        }

        let shaped = shape_shell_output(&args.command, &stdout, &stderr, truncated, exit_code);
        let shaped_stdout =
            append_spillover_footer(&shaped.stdout, spillover.as_ref(), raw_spillover.as_ref());
        let mut content = raw_content;
        if let Some(object) = content.as_object_mut() {
            object.insert("stdout".to_string(), json!(shaped_stdout));
            object.insert("stderr".to_string(), json!(shaped.stderr));
            object.insert(
                "output_shape".to_string(),
                json!({
                    "mode": "shaped",
                    "family": shaped.family,
                    "kind": shaped.kind,
                    "raw_stdout_bytes": stdout.len(),
                    "raw_stderr_bytes": stderr.len(),
                    "shaped_stdout_bytes": shaped_stdout.len(),
                    "shaped_stderr_bytes": shaped.stderr.len(),
                    "raw_output_sha256": raw_output_sha256.clone(),
                    "fallback_reason": shaped.fallback_reason,
                }),
            );
        }
        let mut shaped_result = make_result(call, status, content, cost, None);
        shaped_result.receipt.output_sha256 = raw_output_sha256;
        shaped_result.with_spill_model_output(raw_output)
    }

    async fn run_shell_plan(
        &self,
        request: ShellRunRequest<'_>,
    ) -> std::result::Result<ShellRunOutcome, ShellRunError> {
        let ShellRunRequest {
            sandbox_plan,
            workdir,
            timeout_ms,
            output_cap,
            tty,
            cancel,
            call,
            command_text,
            shell_ask_approver,
        } = request;
        // Spawn strategy. The Windows restricted-token / elevated sandbox child
        // is created via raw Win32 (it cannot go through
        // `tokio::process::Command`: that always uses the caller's token, and
        // Windows has no `pre_exec`). Everything else uses the standard tokio
        // command path. `pty_master`/`ask_server` apply only to the tokio path.
        #[cfg(windows)]
        let win_sandbox_backend = matches!(
            sandbox_plan.backend,
            "windows-restricted-token" | "windows-elevated"
        );
        #[cfg(not(windows))]
        let win_sandbox_backend = false;

        let pty_master: Option<std::fs::File>;
        let ask_server: Option<ShellAskServer>;
        let mut child: ShellChild;
        // Tracking vars set by platform-gated cfg blocks below; `mut` is
        // needed only on the relevant platform but must compile on all targets.
        #[allow(unused_mut)]
        let mut tty_degraded = false;
        #[allow(unused_mut)]
        let mut tty_downgraded = false;
        // Set to Some(reason) when ShellAskServer::start fails on a backend
        // that otherwise supports AF_UNIX ask sockets, so the failure can be
        // surfaced in the shell result JSON as nested_ask: { available: false }.
        #[allow(unused_mut)]
        let mut ask_server_start_error: Option<String> = None;

        if win_sandbox_backend {
            #[cfg(windows)]
            {
                // The Windows sandbox owns its own pipes + scrubbed env; the PTY
                // and `squeezy ask` socket paths do not apply on this backend.
                // ConPTY is not wired for the sandbox path either, so record
                // the same tty_degraded signal as the non-sandbox tokio path.
                if tty {
                    tty_degraded = true;
                    tty_downgraded = true;
                }
                let _ = tty;
                drop(shell_ask_approver);
                let spec = build_win_spec(&self.shell_sandbox, &self.root, sandbox_plan);
                let mut argv = Vec::with_capacity(1 + sandbox_plan.args.len());
                argv.push(sandbox_plan.program.clone());
                argv.extend(sandbox_plan.args.iter().cloned());
                let env = preserved_env_string_map(&self.shell_sandbox, &self.shell_sandbox_health);
                let spawned = if sandbox_plan.backend == "windows-elevated" {
                    squeezy_win_sandbox::spawn_elevated(&spec, &argv, workdir, &env, false)
                } else {
                    squeezy_win_sandbox::spawn_restricted_token(&spec, &argv, workdir, &env, false)
                };
                let win_child = match spawned {
                    Ok(win_child) => win_child,
                    Err(err) if sandbox_plan.required => {
                        return Err(ShellRunError::SandboxStartDenied(format!(
                            "shell sandbox backend {} failed to start: {err}",
                            sandbox_plan.backend
                        )));
                    }
                    Err(err) => {
                        // Non-required Windows sandbox spawn failure: degrade
                        // the same way as the Linux pre_exec path so the
                        // caller can retry on the unsandboxed direct path.
                        return Err(ShellRunError::SpawnFallback(format!(
                            "shell sandbox backend {} spawn failed: {err}",
                            sandbox_plan.backend
                        )));
                    }
                };
                pty_master = None;
                ask_server = None;
                child = ShellChild::WinSandbox(win_child);
            }
            #[cfg(not(windows))]
            {
                unreachable!("windows sandbox backend selected on a non-windows target");
            }
        } else {
            let mut command = Command::new(&sandbox_plan.program);
            command
                .args(&sandbox_plan.args)
                .current_dir(workdir)
                .kill_on_drop(true);
            pty_master = if tty {
                #[cfg(unix)]
                {
                    let pty = open_shell_pty().map_err(ShellRunError::Io)?;
                    command
                        .stdin(Stdio::from(
                            pty.slave.try_clone().map_err(ShellRunError::Io)?,
                        ))
                        .stdout(Stdio::from(
                            pty.slave.try_clone().map_err(ShellRunError::Io)?,
                        ))
                        .stderr(Stdio::from(pty.slave));
                    Some(pty.master)
                }
                #[cfg(not(unix))]
                {
                    // Windows non-sandbox path: ConPTY is not yet wired up;
                    // degrade to non-TTY pipes. The shell still runs with the
                    // requested backend, just without a controlling terminal.
                    // The outcome records `tty_degraded = true` so the caller
                    // can surface a note to the user.
                    tty_degraded = true;
                    tty_downgraded = true;
                    command
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped());
                    None
                }
            } else {
                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                None
            };
            // On non-Unix platforms, tty=true degrades to pipes because
            // ConPTY is not yet wired. Record this so the caller can surface
            // a user-visible note.
            if tty {
                #[cfg(not(unix))]
                {
                    tty_degraded = true;
                    tty_downgraded = true;
                }
            }
            configure_shell_process_group(&mut command);
            configure_linux_shell_sandbox(&mut command, sandbox_plan);
            apply_shell_environment_policy(
                &mut command,
                &self.shell_sandbox,
                &self.shell_sandbox_health,
            );
            // Only export the `squeezy ask` socket when the active backend
            // permits the child's `socket(AF_UNIX, …)` connect. The
            // linux-direct-syscalls seccomp filter denies AF_UNIX sockets, so
            // exporting `SQUEEZY_ASK_SOCKET` there would advertise an escalation
            // path that is guaranteed to fail with a confusing `EPERM`.
            ask_server = if let Some(approver) =
                shell_ask_approver.filter(|_| sandbox_plan.exports_ask_socket())
            {
                match ShellAskServer::start(
                    &self.root,
                    &call.call_id,
                    command_text,
                    workdir,
                    approver,
                    cancel.clone(),
                )
                .await
                {
                    Ok(server) => {
                        command.env(SQUEEZY_ASK_SOCKET_ENV, server.env_value());
                        command.env(SQUEEZY_ASK_CALL_ID_ENV, &call.call_id);
                        Some(server)
                    }
                    Err(err) => {
                        // Record the startup failure so it can be surfaced in
                        // the shell result JSON as `nested_ask: { available:
                        // false, reason: "…" }`.  The child will not have
                        // SQUEEZY_ASK_SOCKET set even though the backend
                        // supports AF_UNIX, so the model and user must know
                        // that nested approvals are unavailable for this call.
                        ask_server_start_error = Some(format!(
                            "nested ask disabled: ask server failed to start: {err}"
                        ));
                        None
                    }
                }
            } else {
                None
            };
            child = match command.spawn() {
                Ok(child) => ShellChild::Tokio(child),
                Err(err) if sandbox_plan.required => {
                    return Err(ShellRunError::SandboxStartDenied(format!(
                        "shell sandbox backend {} failed to start: {err}",
                        sandbox_plan.backend
                    )));
                }
                Err(err) if sandbox_plan.backend != "none" => {
                    // Non-required sandboxed spawn failure — the pre-exec hook
                    // or the process setup was blocked (most commonly the Linux
                    // `unshare`/seccomp pre_exec path blocked by the container's
                    // own seccomp policy). Treat this as a best_effort fallback
                    // rather than a hard I/O error so the caller can retry on
                    // the unsandboxed direct path.
                    return Err(ShellRunError::SpawnFallback(format!(
                        "shell sandbox backend {} spawn failed: {err}",
                        sandbox_plan.backend
                    )));
                }
                Err(err) => return Err(ShellRunError::Io(err)),
            };
        }

        // Windows analog to Unix process groups: a Job Object created with
        // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE kills every descendant when either
        // `terminate(...)` is called or the handle drops at function exit.
        //
        // Both Windows sandbox tiers (restricted-token AND elevated) bind their
        // child + descendants into a kill-on-close Job Object INSIDE
        // `squeezy-win-sandbox` (created CREATE_SUSPENDED → assigned → resumed,
        // so there is no escape race), and `terminate_shell_child` tears that
        // job down via `WinSandboxChild::kill`. So we only need a `ShellJob`
        // here for the non-sandboxed tokio path (e.g. windows_sandbox_level =
        // "disabled"); assigning one to a sandbox child would be redundant.
        //
        // Record creation/assignment status for audit: even when Windows
        // filesystem/network isolation is unavailable, process-tree cleanup
        // should be independently observable — a failed job means
        // timeout/cancel may not kill all descendants.
        #[cfg(windows)]
        let (shell_job, win_job_object_status, windows_job_assigned): (
            Option<ShellJob>,
            &'static str,
            Option<bool>,
        ) = if win_sandbox_backend {
            (None, "sandbox_managed", None)
        } else {
            match ShellJob::new() {
                Ok(job) => {
                    if let Some(pid) = child.id() {
                        match job.assign_process(pid) {
                            Ok(()) => (Some(job), "assigned", Some(true)),
                            Err(_) => (Some(job), "assign_failed", Some(false)),
                        }
                    } else {
                        (Some(job), "pid_unavailable", Some(false))
                    }
                }
                Err(_) => (None, "create_failed", Some(false)),
            }
        };
        #[cfg(not(windows))]
        let _win_job_object_status: &'static str = "not_windows";
        #[cfg(not(windows))]
        let windows_job_assigned: Option<bool> = None;

        let stdout_capture = ShellStreamCapture::default();
        let stderr_capture = ShellStreamCapture::default();
        // One sidecar per shell call, shared by the stdout/stderr readers, so
        // the pre-cap bytes the hard cap would discard land in a single
        // `{call_id}-raw.txt` the model can recover via `read_tool_output`.
        // The handle is cheap to mint and writes nothing until a stream
        // overflows the cap; cloning it across both readers keeps the file
        // append-only-shared. Each reader redacts with its own
        // `StreamRedactor`, so secret/PEM state never crosses streams.
        let raw_sidecar = self.shell_spillover.open_raw_sidecar(&call.call_id);
        let stdout_task = if let Some(master) = pty_master {
            tokio::spawn(read_limited_pipe(
                Some(Box::new(tokio::fs::File::from_std(master))
                    as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
                output_cap,
                stdout_capture.clone(),
                raw_sidecar.clone(),
                Arc::clone(&self.redactor),
            ))
        } else {
            tokio::spawn(read_limited_pipe(
                child.take_stdout(),
                output_cap,
                stdout_capture.clone(),
                raw_sidecar.clone(),
                Arc::clone(&self.redactor),
            ))
        };
        let stderr_task = tokio::spawn(read_limited_pipe(
            child.take_stderr(),
            output_cap,
            stderr_capture.clone(),
            raw_sidecar.clone(),
            Arc::clone(&self.redactor),
        ));

        let status = tokio::select! {
            _ = cancel.cancelled() => {
                let cancel_kill_meta =
                    terminate_shell_child(&mut child, self.shell_sandbox.kill_grace_ms).await;
                #[cfg(windows)]
                let cancel_job_cleanup_ok =
                    windows_job_cleanup_status(shell_job.as_ref(), windows_job_assigned);
                #[cfg(not(windows))]
                let cancel_job_cleanup_ok: Option<bool> = None;
                stdout_task.abort();
                stderr_task.abort();
                drop(ask_server);
                return Err(ShellRunError::Cancelled {
                    kill_meta: cancel_kill_meta,
                    windows_job_cleanup_ok: cancel_job_cleanup_ok,
                });
            }
            result = time::timeout(Duration::from_millis(timeout_ms), child.wait()) => result,
        };

        let timed_out = status.is_err();
        #[cfg(windows)]
        let mut windows_timeout_job_cleanup_ok: Option<bool> = None;
        #[cfg(not(windows))]
        let windows_timeout_job_cleanup_ok: Option<bool> = None;
        #[allow(unused_mut)]
        let mut kill_meta: Option<ShellKillMeta> = None;
        let exit_status = match status {
            Ok(Ok(status)) => Some(status),
            Err(_) => {
                kill_meta =
                    terminate_shell_child(&mut child, self.shell_sandbox.kill_grace_ms).await;
                #[cfg(windows)]
                {
                    windows_timeout_job_cleanup_ok =
                        windows_job_cleanup_status(shell_job.as_ref(), windows_job_assigned);
                }
                None
            }
            Ok(Err(err)) => return Err(ShellRunError::Io(err)),
        };

        let drain_timeout = Duration::from_millis(IO_DRAIN_TIMEOUT_MS);
        let (stdout_result, stderr_result) = tokio::join!(
            drain_or_abort(stdout_task, stdout_capture, drain_timeout),
            drain_or_abort(stderr_task, stderr_capture, drain_timeout),
        );
        let (stdout_bytes, stdout_truncated) = stdout_result.map_err(ShellRunError::Io)?;
        let (stderr_bytes, stderr_truncated) = stderr_result.map_err(ShellRunError::Io)?;
        let (stdout_bytes, stdout_truncated, stderr_bytes, stderr_truncated) = split_shell_output(
            stdout_bytes,
            stdout_truncated,
            stderr_bytes,
            stderr_truncated,
            output_cap,
        );
        drop(ask_server);

        // Both readers have exited (completed or aborted on drain timeout),
        // so the last remaining clone can flush + report the sidecar. It
        // returns `None` when the stream stayed under the cap and nothing was
        // written — the zero-cost path.
        let raw_spillover = match raw_sidecar {
            Some(sidecar) => sidecar.finalize().await,
            None => None,
        };

        Ok(ShellRunOutcome {
            exit_status,
            timed_out,
            stdout_bytes,
            stdout_truncated,
            stderr_bytes,
            stderr_truncated,
            raw_spillover,
            #[cfg(windows)]
            win_job_object_status: Some(win_job_object_status),
            #[cfg(not(windows))]
            win_job_object_status: None,
            tty_degraded,
            tty_downgraded,
            windows_job_assigned,
            windows_timeout_job_cleanup_ok,
            kill_meta,
            ask_server_start_error,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShellArgs {
    pub(crate) command: String,
    pub(crate) workdir: Option<String>,
    pub(crate) timeout_ms: Option<u64>,
    pub(crate) output_byte_cap: Option<usize>,
    pub(crate) output_mode: Option<OutputMode>,
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) tty: bool,
    #[serde(default)]
    pub(crate) direct_user_shell: bool,
    /// Per-process secret that the user-driven `!cmd` path mints alongside
    /// `direct_user_shell=true`. Validated against [`direct_user_shell_nonce`]
    /// before the sandbox is disabled; without it, the `local-shell-` call_id
    /// prefix is meaningless and the call falls through to the normal model
    /// path. Never advertised in the shell schema — the model has no way to
    /// observe it, and replay tapes / mock providers in a separate process
    /// will not have a matching value.
    #[serde(default)]
    pub(crate) direct_user_shell_nonce: Option<String>,
}

/// Per-process secret bound to the TUI's local-shell path.
///
/// The hash inputs (PID, wall + monotonic clock samples, heap and static
/// addresses) are visible only inside this process, so the digest cannot be
/// reproduced by any external caller — including mock providers, replay
/// tapes, and out-of-process MCP shims that might one day mint `local-shell-`
/// call_ids. The `direct_user_shell` fast path requires both the call_id
/// prefix and a matching nonce; the prefix alone is therefore insufficient
/// to disable the sandbox or skip checkpointing.
pub fn direct_user_shell_nonce() -> &'static str {
    static NONCE: OnceLock<String> = OnceLock::new();
    NONCE.get_or_init(|| {
        let mut seed = Vec::with_capacity(96);
        seed.extend_from_slice(&std::process::id().to_le_bytes());
        let now_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        seed.extend_from_slice(&now_nanos.to_le_bytes());
        let mono_nanos = std::time::Instant::now().elapsed().as_nanos();
        seed.extend_from_slice(&mono_nanos.to_le_bytes());
        // Heap allocation address is randomized per-process by ASLR on every
        // supported platform; reading it as additional entropy means an
        // attacker who guesses the clocks/PID still cannot reproduce the hash.
        let heap_marker: Box<u8> = Box::new(0);
        let heap_addr = (Box::as_ref(&heap_marker) as *const u8) as usize;
        seed.extend_from_slice(&heap_addr.to_le_bytes());
        // Mix in the address of the OnceLock itself; static address is also
        // ASLR-randomized and differs from heap.
        let lock_addr = (&NONCE as *const OnceLock<String>) as usize;
        seed.extend_from_slice(&lock_addr.to_le_bytes());
        sha256_hex(seed)
    })
}

/// Length-aware byte-for-byte comparison that does not short-circuit on the
/// first mismatch. The nonce is per-process and never travels off-host, so a
/// timing oracle is not really in scope, but constant-time matches the secret
/// shape and avoids future regressions if the comparison ever moves to a
/// boundary where timing matters.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Render the structured spillover metadata the shell tool surfaces in
/// the result content. The `path` is the absolute spillover location
/// (under `$TMPDIR/squeezy-spillover/<session>/`) and `bytes` is the
/// payload size — both are model-consumable arguments for the
/// `read_tool_output` tool.
pub(crate) fn shell_spillover_metadata(info: &ShellSpilloverInfo) -> serde_json::Value {
    json!({
        "path": info.path.to_string_lossy(),
        "bytes": info.bytes,
        "format": "stdout-then-stderr",
    })
}

/// Append the model-facing spillover footer line to a shaped stdout
/// block. The footer is a stable text marker so the model can spot the
/// spillover even when the structured `spillover` field gets stripped
/// during further compaction. It names `read_tool_output` and embeds
/// the path as a copy-pasteable usage example so the model can pivot
/// directly to the recovery call without inferring the contract.
pub(crate) fn append_spillover_footer(
    shaped_stdout: &str,
    spillover: Option<&ShellSpilloverInfo>,
    raw_spillover: Option<&ShellSpilloverInfo>,
) -> String {
    let mut footer = String::new();
    // The capped spillover mirrors the bytes that survived the in-memory cap.
    if let Some(spill) = spillover {
        let path = spill.path.display();
        let bytes = spill.bytes;
        footer.push_str(&format!(
            "[truncated; full output: {path} ({bytes} bytes); recover via read_tool_output {{\"path\": \"{path}\"}}]"
        ));
    }
    // The raw sidecar carries the *complete* pre-cap output (the bytes the
    // hard cap discarded). It is a strict superset of the capped spillover,
    // so name it on its own line as the way to recover everything.
    if let Some(raw) = raw_spillover {
        let path = raw.path.display();
        let bytes = raw.bytes;
        if !footer.is_empty() {
            footer.push('\n');
        }
        footer.push_str(&format!(
            "[full pre-cap output: {path} ({bytes} bytes); recover via read_tool_output {{\"path\": \"{path}\"}}]"
        ));
    }
    if footer.is_empty() {
        return shaped_stdout.to_string();
    }
    if shaped_stdout.is_empty() {
        return footer;
    }
    let needs_newline = !shaped_stdout.ends_with('\n');
    let separator = if needs_newline { "\n" } else { "" };
    format!("{shaped_stdout}{separator}{footer}")
}

pub(crate) fn shell_termination_reason(
    timed_out: bool,
    timeout_ms: u64,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
) -> Option<String> {
    if timed_out {
        return Some(format!("shell command timed out after {timeout_ms} ms"));
    }
    if exit_code.is_some() {
        return None;
    }
    exit_signal
        .map(|signal| format!("shell command terminated by signal {signal}"))
        .or_else(|| Some("shell command ended without an exit code".to_string()))
}

/// Build the error message string for a cancelled shell audit row.
/// When kill metadata is available (Unix only), the message includes the
/// process group id and key signal outcomes so the audit record captures
/// whether pgid-based cleanup reached the child group. `grace_expired` is
/// included for symmetry with the JSON `kill_meta.grace_expired` field so a
/// reader of the audit row can distinguish "child exited inside the SIGTERM
/// grace window" from "grace expired and SIGKILL was needed".
pub(crate) fn shell_cancelled_error_msg(kill_meta: Option<&ShellKillMeta>) -> String {
    match kill_meta {
        None => "shell command cancelled".to_string(),
        Some(km) => {
            let sigkill = if km.sigkill_sent {
                format!(", sigkill_ok={}", km.sigkill_ok)
            } else {
                String::new()
            };
            format!(
                "shell command cancelled: pgid={}, sigterm_ok={}, grace_expired={}{}",
                km.pgid, km.sigterm_ok, km.grace_expired, sigkill,
            )
        }
    }
}

pub(crate) fn shell_command_needs_checkpoint(
    direct_user_shell: bool,
    analysis: &ShellPermissionAnalysis,
) -> bool {
    if direct_user_shell {
        return false;
    }
    match analysis.capability {
        PermissionCapability::Read | PermissionCapability::Search => false,
        PermissionCapability::Git
            if analysis.risk == PermissionRisk::Low
                && !analysis.destructive
                && !analysis.network
                && !analysis.dynamic =>
        {
            false
        }
        _ => true,
    }
}

/// Build a cancelled `ToolResult` for a shell call. When `windows_job_cleanup_ok`
/// is `Some(false)` the Job Object cleanup failed and descendants may survive,
/// so a degraded-cleanup note is included in the result to avoid silent
/// process leakage.
pub(crate) fn shell_cancelled_result(
    call: &ToolCall,
    windows_job_cleanup_ok: Option<bool>,
    kill_meta: Option<ShellKillMeta>,
) -> ToolResult {
    if windows_job_cleanup_ok == Some(false) || kill_meta.is_some() {
        let mut content = serde_json::Map::new();
        content.insert("error".to_string(), json!("tool call cancelled"));
        if let Some(km) = kill_meta {
            content.insert(
                "kill_meta".to_string(),
                json!({
                    "pgid": km.pgid,
                    "sigterm_ok": km.sigterm_ok,
                    "grace_expired": km.grace_expired,
                    "sigkill_sent": km.sigkill_sent,
                    "sigkill_ok": km.sigkill_ok,
                    "direct_child_fallback": km.direct_child_fallback,
                    "direct_child_kill_ok": km.direct_child_kill_ok,
                }),
            );
        }
        if windows_job_cleanup_ok == Some(false) {
            content.insert(
                "windows".to_string(),
                json!({
                    "job_object_cleanup": "failed",
                    "note": "process tree cleanup not guaranteed; descendant processes may still be running"
                }),
            );
        }
        return make_result(
            call,
            ToolStatus::Cancelled,
            serde_json::Value::Object(content),
            ToolCostHint::default(),
            None,
        );
    }
    ToolResult::cancelled(call)
}

pub(crate) fn shell_policy_denied(
    call: &ToolCall,
    analysis: &ShellPermissionAnalysis,
    reason: impl Into<String>,
) -> ToolResult {
    make_result(
        call,
        ToolStatus::Denied,
        json!({
            "error": reason.into(),
            "permission_denied": true,
            "policy_denied": true,
            "capability": analysis.capability.as_str(),
            "target": analysis.rule_target,
            "risk": analysis.risk.as_str(),
            "network": if analysis.network { "detected" } else { "none" },
            "destructive": analysis.destructive,
            "parser_backed": analysis.parser_backed,
            "dynamic": analysis.dynamic,
        }),
        ToolCostHint::default(),
        None,
    )
}

/// Check whether the command text references any configured sensitive path
/// pattern. The matcher splits the command into tokens (respecting quotes),
/// normalises each token (expands `~` and `$HOME` against the parent env,
/// collapses `\\` to `/`), and then tests each token against each pattern's
/// base. This avoids the original implementation's substring-in-haystack
/// problem (where `.env*` matched any string containing `.env`, including
/// unrelated package or option names like `.environment`), while still
/// catching common bypasses like `$HOME/.ssh/id_rsa` and `~/.aws/config`.
pub(crate) fn shell_command_references_sensitive_path(
    command: &str,
    patterns: &[String],
) -> Option<String> {
    let tokens = tokenize_shell_segment(command);
    let home = env::var_os("HOME").map(|s| s.to_string_lossy().into_owned());
    for raw in &tokens {
        let stripped = dequote_token(raw);
        let normalized = normalize_path_token(stripped, home.as_deref());
        for pattern in patterns {
            let base = sensitive_pattern_base(pattern);
            if !base.is_empty() && token_contains_sensitive_base(&normalized, &base) {
                return Some(pattern.clone());
            }
        }
    }
    // Backstop: also scan the raw command (with backslashes normalised)
    // for unquoted occurrences of each pattern base preceded by a path
    // separator. This catches uses like `tar --exclude='*.cache' .ssh/`
    // and unquoted `cat ~/.ssh/id_rsa`.
    let normalized_command = command.replace('\\', "/");
    for pattern in patterns {
        let base = sensitive_pattern_base(pattern);
        if base.is_empty() {
            continue;
        }
        if normalized_command_references_base(&normalized_command, &base) {
            return Some(pattern.clone());
        }
    }
    None
}

fn shell_command_references_protected_metadata(
    command: &str,
    protected_names: &[String],
) -> Option<String> {
    if protected_names.is_empty() {
        return None;
    }
    let tokens = tokenize_shell_segment(command);
    for raw in &tokens {
        let normalized = dequote_token(raw).replace('\\', "/");
        for part in normalized.split('/') {
            if protected_names.iter().any(|name| name == part) {
                return Some(part.to_string());
            }
        }
    }
    let normalized_command = command.replace('\\', "/");
    for name in protected_names {
        if normalized_command
            .split_whitespace()
            .any(|token| token.split('/').any(|part| part.trim_matches('"') == name))
        {
            return Some(name.clone());
        }
    }
    None
}

fn shell_command_writes_protected_metadata(
    command: &str,
    protected_names: &[String],
) -> Option<String> {
    let name = shell_command_references_protected_metadata(command, protected_names)?;
    let raw_segments = match parse_shell_command(command) {
        Some(parsed) if !parsed.segments.is_empty() => parsed.segments,
        _ => shell_segments(command),
    };
    let segments = expand_wrapper_segments(raw_segments);
    if segments
        .iter()
        .any(|segment| shell_segment_writes_filesystem(segment))
    {
        Some(name)
    } else {
        None
    }
}

pub(crate) fn shell_segment_writes_filesystem(segment: &str) -> bool {
    if is_destructive_shell_segment(segment) {
        return true;
    }
    let tokens = tokenize_shell_segment(segment)
        .into_iter()
        .map(|token| dequote_token(&token).to_string())
        .collect::<Vec<_>>();
    let first = tokens.first().map(String::as_str).unwrap_or("");
    if matches!(
        first,
        "chmod" | "cp" | "install" | "ln" | "mkdir" | "mktemp" | "mv" | "rsync" | "tee" | "touch"
    ) {
        return true;
    }
    first == "sed"
        && tokens
            .iter()
            .any(|token| token == "-i" || token.starts_with("-i."))
}

/// Normalises a path-like token for sensitive-path matching:
///   - replaces backslashes with `/`,
///   - expands a leading `~/` or `~` against `$HOME`,
///   - expands a leading `$HOME` or `${HOME}` against `$HOME`,
///   - expands Windows `%VAR%` and PowerShell `$env:VAR` forms for
///     `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, and `HOME`.
fn normalize_path_token(token: &str, home: Option<&str>) -> String {
    // Normalise path separators first so all comparisons use `/`.
    let token = token.replace('\\', "/");

    // Expand Unix-style $HOME / ${HOME} / ~/
    if let Some(home) = home {
        if let Some(rest) = token.strip_prefix("$HOME/") {
            return format!("{home}/{rest}");
        }
        if token == "$HOME" {
            return home.to_string();
        }
        if let Some(rest) = token.strip_prefix("${HOME}/") {
            return format!("{home}/{rest}");
        }
        if token == "${HOME}" {
            return home.to_string();
        }
        if let Some(rest) = token.strip_prefix("~/") {
            return format!("{home}/{rest}");
        }
        if token == "~" {
            return home.to_string();
        }
    }

    // Expand Windows cmd-style `%VAR%` and PowerShell `$env:VAR` prefixes.
    // We only expand the three most security-sensitive Windows path roots so
    // the pattern list stays specific. The result is normalised to `/`
    // separators so the subsequent token_contains_sensitive_base check works
    // identically on all platforms.
    for (cmd_var, ps_var, env_key) in [
        ("%USERPROFILE%", "$env:USERPROFILE", "USERPROFILE"),
        ("%APPDATA%", "$env:APPDATA", "APPDATA"),
        ("%LOCALAPPDATA%", "$env:LOCALAPPDATA", "LOCALAPPDATA"),
        ("%HOME%", "$env:HOME", "HOME"),
    ] {
        let value = env::var_os(env_key)
            .map(|v| v.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if value.is_empty() {
            continue;
        }
        for prefix in [cmd_var, ps_var] {
            // Both cmd.exe (`%USERPROFILE%`) and PowerShell (`$env:USERPROFILE`)
            // treat env-var names case-insensitively, so match case-insensitively
            // for both the bare form and the path form (e.g. `%userprofile%\...`).
            let prefix_lower = prefix.to_ascii_lowercase();
            let token_lower = token.to_ascii_lowercase();
            let prefix_slash_lower = format!("{prefix_lower}/");
            if let Some(rest) = token_lower.strip_prefix(&prefix_slash_lower) {
                // Preserve original-case suffix from the real token.
                let rest_start = token.len() - rest.len();
                return format!("{value}/{}", &token[rest_start..]);
            }
            if token_lower == prefix_lower {
                return value.clone();
            }
        }
    }

    token
}

/// Token-side check: does `token` reference `base` either as a path
/// segment or as an exact match? Avoids matching `.env` inside
/// `.environment` or `Cargo.envelope`.
fn token_contains_sensitive_base(token: &str, base: &str) -> bool {
    if token == base {
        return true;
    }
    // Strip leading `/` so absolute and relative both compare segment-wise.
    let token = token.trim_start_matches('/');
    let base = base.trim_end_matches('/');
    for piece in token.split('/') {
        if piece == base {
            return true;
        }
        // Trailing wildcard support for patterns like `.env*` → base
        // `.env`: require the segment to begin with `.env.` or `.env-`
        // or be exactly `.env`, not match `.environment`.
        if let Some(rest) = piece.strip_prefix(base)
            && (rest.is_empty()
                || rest.starts_with('.')
                || rest.starts_with('-')
                || rest.starts_with('_'))
        {
            return true;
        }
    }
    false
}

/// Command-side check: matches `base` when preceded by a path separator
/// (or appearing at the start of a token). Handles unquoted uses like
/// `tar -czf out.tgz ~/.ssh` even when the tokenizer would otherwise
/// have split `~/.ssh` away from the path matcher.
fn normalized_command_references_base(command: &str, base: &str) -> bool {
    let needles = [format!("/{base}"), format!(" {base}"), format!("\t{base}")];
    for needle in &needles {
        if let Some(idx) = command.find(needle.as_str()) {
            let next = command[idx + needle.len()..].chars().next();
            if next
                .map(|c| matches!(c, '/' | ' ' | '\t' | '\0' | '"' | '\''))
                .unwrap_or(true)
            {
                return true;
            }
            // Allow segment-style follow-ups (e.g. `.env.production`).
            if next.map(|c| matches!(c, '.' | '-' | '_')).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

#[cfg(unix)]
struct ShellPty {
    master: fs::File,
    slave: fs::File,
}

#[cfg(unix)]
fn open_shell_pty() -> std::io::Result<ShellPty> {
    let mut master = -1;
    let mut slave = -1;
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(ShellPty {
        master: unsafe { fs::File::from_raw_fd(master) },
        slave: unsafe { fs::File::from_raw_fd(slave) },
    })
}

/// A spawned shell child. Abstracts over the standard `tokio` process (macOS,
/// Linux, and the non-sandboxed Windows path) and the Windows restricted-token
/// / elevated sandbox child, which is spawned via raw Win32 and therefore
/// cannot be a `tokio::process::Child` (that type always uses the caller's
/// token and Windows has no `pre_exec`). The capture / timeout / cancel loop in
/// `run_shell_plan` drives every child through this uniform surface.
enum ShellChild {
    Tokio(tokio::process::Child),
    #[cfg(windows)]
    WinSandbox(squeezy_win_sandbox::WinSandboxChild),
}

impl ShellChild {
    // Only consulted by the Windows Job Object assignment.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn id(&self) -> Option<u32> {
        match self {
            ShellChild::Tokio(child) => child.id(),
            #[cfg(windows)]
            ShellChild::WinSandbox(child) => Some(child.id()),
        }
    }

    fn take_stdout(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>> {
        match self {
            ShellChild::Tokio(child) => child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
            #[cfg(windows)]
            ShellChild::WinSandbox(child) => child
                .take_stdout()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        }
    }

    fn take_stderr(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>> {
        match self {
            ShellChild::Tokio(child) => child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
            #[cfg(windows)]
            ShellChild::WinSandbox(child) => child
                .take_stderr()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        }
    }

    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match self {
            ShellChild::Tokio(child) => child.wait().await,
            #[cfg(windows)]
            ShellChild::WinSandbox(child) => child.wait().await,
        }
    }
}

async fn terminate_shell_child(child: &mut ShellChild, grace_ms: u64) -> Option<ShellKillMeta> {
    match child {
        ShellChild::Tokio(child) => {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                // `configure_shell_process_group` creates a dedicated process
                // group whose pgid is the child pid. Keep reporting the same
                // pgid we signal so metadata reflects the actual target.
                let pgid = pid;
                let sigterm_ok = kill_process_group(pgid, libc::SIGTERM);
                // Treat the grace-period wait as "not expired" only when the
                // outer timeout returned Ok(Ok(_)) — meaning the child actually
                // exited.  Ok(Err(_)) means child.wait() returned an IO error
                // (e.g. ECHILD if the child was already reaped by a signal
                // handler). In that case we conservatively treat the grace as
                // expired and escalate to SIGKILL; the kill may be a no-op if
                // the child is already gone, but that is safe.
                let grace_wait = time::timeout(Duration::from_millis(grace_ms), child.wait()).await;
                let grace_expired = !matches!(grace_wait, Ok(Ok(_)));
                if !grace_expired {
                    // Child exited cleanly within the grace period; no SIGKILL
                    // or direct-child fallback is needed.
                    return Some(ShellKillMeta {
                        pgid,
                        sigterm_ok,
                        grace_expired: false,
                        sigkill_sent: false,
                        sigkill_ok: false,
                        direct_child_fallback: false,
                        direct_child_kill_ok: false,
                    });
                }
                let sigkill_ok = kill_process_group(pgid, libc::SIGKILL);
                // Capture the supplemental direct-child kill result so the
                // metadata can distinguish "we attempted the fallback" from
                // "the fallback reached a live process". A common `false`
                // outcome is tokio returning `InvalidInput` because the prior
                // pgid SIGKILL already reaped the direct child.
                let direct_child_kill_ok = child.kill().await.is_ok();
                let _ = child.wait().await;
                return Some(ShellKillMeta {
                    pgid,
                    sigterm_ok,
                    grace_expired: true,
                    sigkill_sent: true,
                    sigkill_ok,
                    direct_child_fallback: true,
                    direct_child_kill_ok,
                });
            }
            #[cfg(not(unix))]
            let _ = grace_ms;
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        #[cfg(windows)]
        ShellChild::WinSandbox(child) => {
            let _ = grace_ms;
            child.kill();
            let _ = child.wait().await;
        }
    }
    None
}

/// Send `signal` to the process group identified by `pgid`.
/// Returns `true` when the `kill(2)` syscall succeeded (return value ≥ 0).
/// A `false` return means the pgid was not reachable — the process may have
/// already exited, changed its process group via `setsid`, or the caller
/// lacked permission. The caller should treat `false` as a best-effort
/// signal attempt, not a hard error; `child.kill()` follows as a fallback.
#[cfg(unix)]
fn kill_process_group(pgid: u32, signal: libc::c_int) -> bool {
    unsafe { libc::kill(-(pgid as libc::pid_t), signal) >= 0 }
}

/// Compute the env-allowlist-filtered environment that a sandboxed shell child
/// should inherit. Shared by the tokio command path
/// ([`apply_shell_environment_policy`]) and the Windows sandbox spawn path
/// ([`preserved_env_string_map`]) so both apply identical env scrubbing.
/// When a `health` cache is provided the result is memoised on the first call
/// and cloned on subsequent calls, avoiding a full `vars_os()` scan per shell
/// invocation (the allowlist and process environment are both stable per
/// session).
fn compute_preserved_env(
    config: &ShellSandboxConfig,
    health: &crate::shell_sandbox::ShellSandboxHealth,
) -> BTreeMap<String, OsString> {
    health
        .preserved_env_cache
        .get_or_init(|| {
            let mut preserved = BTreeMap::<String, OsString>::new();
            for (name, value) in env::vars_os() {
                let Some(name) = name.to_str() else {
                    continue;
                };
                if shell_env_should_preserve(name, &config.env_allowlist) {
                    preserved.insert(name.to_string(), value);
                }
            }
            preserved
        })
        .clone()
}

fn apply_shell_environment_policy(
    command: &mut Command,
    config: &ShellSandboxConfig,
    health: &crate::shell_sandbox::ShellSandboxHealth,
) {
    let preserved = compute_preserved_env(config, health);
    command.env_clear();
    for (name, value) in &preserved {
        command.env(name, value);
    }
}

/// The Windows sandbox spawn path takes a fully-formed environment block rather
/// than mutating a `Command`, so flatten the allowlisted environment into the
/// `HashMap<String, String>` the crate expects (lossy for the rare non-UTF-16
/// value, which is acceptable for a scrubbed sandbox environment).
#[cfg(windows)]
fn preserved_env_string_map(
    config: &ShellSandboxConfig,
    health: &crate::shell_sandbox::ShellSandboxHealth,
) -> std::collections::HashMap<String, String> {
    compute_preserved_env(config, health)
        .into_iter()
        .map(|(name, value)| (name, value.to_string_lossy().into_owned()))
        .collect()
}

pub(crate) fn shell_env_should_preserve(name: &str, allowlist: &[String]) -> bool {
    allowlist.iter().any(|pattern| {
        if let Some(prefix) = pattern.strip_suffix('*') {
            name.starts_with(prefix)
        } else {
            name == pattern
        }
    })
}
