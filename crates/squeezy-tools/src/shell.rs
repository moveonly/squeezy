use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[cfg(unix)]
use std::fs;

#[cfg(unix)]
use std::os::fd::FromRawFd;

use serde::Deserialize;
use serde_json::json;
use squeezy_core::{
    PermissionCapability, PermissionRisk, ShellSandboxConfig, ShellSandboxMode,
    sensitive_pattern_base,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{Mutex, OwnedMutexGuard, OwnedSemaphorePermit},
    time,
};
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
use crate::ipc;
use crate::ipc::IpcListener;
#[cfg(unix)]
use crate::sha256_hex;
use crate::shell_output::{insert_content_field, shape_shell_output};
use crate::shell_parse::{
    analyze_shell_command, dequote_token, expand_wrapper_segments, is_destructive_shell_segment,
    parse_shell_command, shell_coverage_warnings, shell_segments, tokenize_shell_segment,
};
use crate::shell_sandbox::{
    ShellSandboxPlan, configure_linux_shell_sandbox, configure_shell_process_group,
    shell_sandbox_best_effort_fallback_reason, shell_sandbox_runtime_unavailable,
    shell_sandbox_status_metadata,
};
#[cfg(windows)]
use crate::win_job::ShellJob;
use crate::{
    DEFAULT_SHELL_OUTPUT_BYTE_CAP, DEFAULT_SHELL_TIMEOUT_MS, IO_DRAIN_TIMEOUT_MS, IpcEndpoint,
    IpcStream, MAX_SHELL_OUTPUT_BYTE_CAP, MAX_SHELL_TIMEOUT_MS, OutputMode,
    SQUEEZY_ASK_CALL_ID_ENV, SQUEEZY_ASK_SOCKET_ENV, ShellAskApprover, ShellAskDecision,
    ShellAskRequest, ShellPermissionAnalysis, ToolCall, ToolCostHint, ToolRegistry, ToolResult,
    ToolStatus, make_result, shell_exit_signal, tool_arg_error, tool_error,
};

pub(crate) struct ShellRunOutcome {
    pub(crate) exit_status: Option<std::process::ExitStatus>,
    pub(crate) timed_out: bool,
    pub(crate) stdout_bytes: Vec<u8>,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_bytes: Vec<u8>,
    pub(crate) stderr_truncated: bool,
    pub(crate) preserved_env: Vec<String>,
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
    Cancelled,
    SandboxStartDenied(String),
    Io(std::io::Error),
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
        let direct_user_shell = args.direct_user_shell && call.call_id.starts_with("local-shell-");
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
        let implicit_skill = self.skills.detect_for_command(&args.command, &workdir);
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
            && self.checkpoints.is_some()
        {
            match self.track_checkpoint_tree() {
                Ok(snapshot) => snapshot,
                Err(err) => return tool_error(call, err),
            }
        } else {
            None
        };
        let coverage_warnings = shell_coverage_warnings(&args.command);

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
            Err(ShellRunError::Cancelled) => {
                self.audit_shell(
                    call,
                    &args,
                    &workdir,
                    &analysis,
                    sandbox_plan.metadata(),
                    timeout_ms,
                    output_cap,
                    "cancelled",
                    Some("shell command cancelled"),
                    None,
                    &[],
                    &[],
                );
                return ToolResult::cancelled(call);
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
            Err(ShellRunError::Io(err)) => return tool_error(call, err),
        };
        if let Some(reason) = shell_sandbox_best_effort_fallback_reason(&sandbox_plan, &run) {
            let exit_code = run.exit_status.as_ref().and_then(|status| status.code());
            self.audit_shell(
                call,
                &args,
                &workdir,
                &analysis,
                sandbox_plan.metadata(),
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
            sandbox_plan = ShellSandboxPlan::direct_with_fallback(
                &args.command,
                self.shell_sandbox.mode,
                &self.shell_sandbox,
                Some(reason),
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
                Err(ShellRunError::Cancelled) => {
                    self.audit_shell(
                        call,
                        &args,
                        &workdir,
                        &analysis,
                        sandbox_plan.metadata(),
                        timeout_ms,
                        output_cap,
                        "cancelled",
                        Some("shell command cancelled"),
                        None,
                        &[],
                        &[],
                    );
                    return ToolResult::cancelled(call);
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
            preserved_env,
        } = run;

        let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
        let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
        let redacted_stdout = self.redactor.redact(&stdout);
        let redacted_stderr = self.redactor.redact(&stderr);
        let stdout = redacted_stdout.text;
        let stderr = redacted_stderr.text;
        let truncated = stdout_truncated || stderr_truncated || timed_out;
        let cost = ToolCostHint {
            output_bytes: (stdout.len() + stderr.len()) as u64,
            redactions: redacted_stdout.redactions + redacted_stderr.redactions,
            truncated,
            ..ToolCostHint::default()
        };
        let exit_code = exit_status.as_ref().and_then(|status| status.code());
        let exit_signal = shell_exit_signal(exit_status.as_ref());
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
                sandbox_plan.metadata(),
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
            sandbox_plan.metadata(),
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

        let mut raw_content = json!({
            "command": args.command,
            "workdir": self.relative(&workdir).to_string_lossy(),
            "exit_code": exit_code,
            "signal": exit_signal,
            "termination": termination,
            "stdout": stdout,
            "stderr": stderr,
            "error": error,
            "truncated": truncated,
            "policy": {
                "capability": analysis.capability.as_str(),
                "target": analysis.rule_target,
                "risk": analysis.risk.as_str(),
                "network": if analysis.network { "classified" } else { "none" },
                "destructive": analysis.destructive,
                "parser_backed": analysis.parser_backed,
                "dynamic": analysis.dynamic,
                "direct_user_shell": direct_user_shell,
                "tty": args.tty,
                "timeout_ms": timeout_ms,
                "output_byte_cap": output_cap,
            },
            "sandbox": sandbox_plan.metadata(),
            "env": {
                "policy": "allowlist",
                "values": "redacted",
                "preserved": preserved_env,
            },
        });
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
        if let Some(checkpoint_before) = checkpoint_before.as_ref() {
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
        let mut content = raw_content;
        if let Some(object) = content.as_object_mut() {
            object.insert("stdout".to_string(), json!(shaped.stdout));
            object.insert("stderr".to_string(), json!(shaped.stderr));
            object.insert(
                "output_shape".to_string(),
                json!({
                    "mode": "shaped",
                    "family": shaped.family,
                    "kind": shaped.kind,
                    "raw_stdout_bytes": stdout.len(),
                    "raw_stderr_bytes": stderr.len(),
                    "shaped_stdout_bytes": shaped.stdout.len(),
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
        let mut command = Command::new(&sandbox_plan.program);
        command
            .args(&sandbox_plan.args)
            .current_dir(workdir)
            .kill_on_drop(true);
        let pty_master = if tty {
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
                // Windows: ConPTY is not yet wired up; degrade to non-TTY
                // pipes. The shell still runs with the requested sandbox
                // backend, just without an allocated controlling terminal.
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
        configure_shell_process_group(&mut command);
        configure_linux_shell_sandbox(&mut command, sandbox_plan);
        let mut preserved_env = apply_shell_environment_policy(&mut command, &self.shell_sandbox);
        let ask_server = if let Some(approver) = shell_ask_approver {
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
                    preserved_env.push(SQUEEZY_ASK_SOCKET_ENV.to_string());
                    preserved_env.push(SQUEEZY_ASK_CALL_ID_ENV.to_string());
                    Some(server)
                }
                Err(_err) => None,
            }
        } else {
            None
        };
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) if sandbox_plan.required => {
                return Err(ShellRunError::SandboxStartDenied(format!(
                    "shell sandbox backend {} failed to start: {err}",
                    sandbox_plan.backend
                )));
            }
            Err(err) => return Err(ShellRunError::Io(err)),
        };
        // Windows analog to Unix process groups: a Job Object created with
        // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE kills every descendant when
        // either `terminate(...)` is called or the handle drops at
        // function exit.
        #[cfg(windows)]
        let shell_job: Option<ShellJob> = match ShellJob::new() {
            Ok(job) => {
                if let Some(pid) = child.id() {
                    let _ = job.assign_process(pid);
                }
                Some(job)
            }
            Err(_) => None,
        };

        let stdout_capture = ShellStreamCapture::default();
        let stderr_capture = ShellStreamCapture::default();
        let stdout_task = if let Some(master) = pty_master {
            tokio::spawn(read_limited_pipe(
                Some(tokio::fs::File::from_std(master)),
                output_cap,
                stdout_capture.clone(),
            ))
        } else {
            tokio::spawn(read_limited_pipe(
                child.stdout.take(),
                output_cap,
                stdout_capture.clone(),
            ))
        };
        let stderr_task = tokio::spawn(read_limited_pipe(
            child.stderr.take(),
            output_cap,
            stderr_capture.clone(),
        ));

        let status = tokio::select! {
            _ = cancel.cancelled() => {
                terminate_shell_child(&mut child, self.shell_sandbox.kill_grace_ms).await;
                #[cfg(windows)]
                if let Some(job) = shell_job.as_ref() {
                    let _ = job.terminate(1);
                }
                stdout_task.abort();
                stderr_task.abort();
                drop(ask_server);
                return Err(ShellRunError::Cancelled);
            }
            result = time::timeout(Duration::from_millis(timeout_ms), child.wait()) => result,
        };

        let timed_out = status.is_err();
        let exit_status = match status {
            Ok(Ok(status)) => Some(status),
            Err(_) => {
                terminate_shell_child(&mut child, self.shell_sandbox.kill_grace_ms).await;
                #[cfg(windows)]
                if let Some(job) = shell_job.as_ref() {
                    let _ = job.terminate(1);
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

        Ok(ShellRunOutcome {
            exit_status,
            timed_out,
            stdout_bytes,
            stdout_truncated,
            stderr_bytes,
            stderr_truncated,
            preserved_env,
        })
    }
}

#[derive(Debug, Deserialize)]
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
    let parsed = parse_shell_command(command);
    let raw_segments = parsed
        .as_ref()
        .map(|parsed| parsed.segments.clone())
        .filter(|segments| !segments.is_empty())
        .unwrap_or_else(|| shell_segments(command));
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
///   - expands a leading `$HOME` or `${HOME}` against `$HOME`.
fn normalize_path_token(token: &str, home: Option<&str>) -> String {
    let token = token.replace('\\', "/");
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

struct ShellAskServer {
    endpoint: IpcEndpoint,
    task: tokio::task::JoinHandle<()>,
}

impl ShellAskServer {
    async fn start(
        root: &Path,
        call_id: &str,
        parent_command: &str,
        workdir: &Path,
        approver: ShellAskApprover,
        cancel: CancellationToken,
    ) -> std::io::Result<Self> {
        let sanitized = sanitize_shell_call_id(call_id);
        #[cfg(unix)]
        {
            let run_dir = root.join(".squeezy").join("run");
            fs::create_dir_all(&run_dir)?;
        }
        let primary = IpcEndpoint::for_shell_ask(root, &sanitized);
        let (endpoint, listener) = match IpcListener::bind(&primary) {
            Ok(listener) => (primary, listener),
            #[cfg(unix)]
            Err(err) if ipc::is_path_too_long(&err) => {
                let digest = sha256_hex(format!("{}:{call_id}", root.display()));
                let fallback = IpcEndpoint::unix_short_fallback(&digest[..16]);
                let listener = IpcListener::bind(&fallback)?;
                (fallback, listener)
            }
            Err(err) => return Err(err),
        };
        let call_id = call_id.to_string();
        let parent_command = parent_command.to_string();
        let workdir = workdir.to_path_buf();
        let task = tokio::spawn(async move {
            shell_ask_server_loop(listener, call_id, parent_command, workdir, approver, cancel)
                .await;
        });
        Ok(Self { endpoint, task })
    }

    fn env_value(&self) -> std::ffi::OsString {
        self.endpoint.as_env_value()
    }
}

impl Drop for ShellAskServer {
    fn drop(&mut self) {
        self.task.abort();
        // Synchronously remove the Unix sock so callers that observe the
        // path immediately after server-drop see it gone. Tokio's task
        // abort is async — relying on `IpcListener::Drop` inside the
        // spawned future races with the assertion. No-op on Windows.
        self.endpoint.remove_local_artifacts();
    }
}

#[derive(Debug, Deserialize)]
struct ShellAskWireRequest {
    command: String,
    justification: String,
}

async fn shell_ask_server_loop(
    listener: IpcListener,
    call_id: String,
    parent_command: String,
    workdir: PathBuf,
    approver: ShellAskApprover,
    cancel: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let Ok(stream) = accepted else {
            break;
        };
        let request_call_id = call_id.clone();
        let request_parent = parent_command.clone();
        let request_workdir = workdir.clone();
        let request_approver = approver.clone();
        tokio::spawn(async move {
            let _ = handle_shell_ask_client(
                stream,
                request_call_id,
                request_parent,
                request_workdir,
                request_approver,
            )
            .await;
        });
    }
}

async fn handle_shell_ask_client(
    mut stream: IpcStream,
    call_id: String,
    parent_command: String,
    workdir: PathBuf,
    approver: ShellAskApprover,
) -> std::io::Result<()> {
    const MAX_ASK_REQUEST_BYTES: usize = 16 * 1024;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() > MAX_ASK_REQUEST_BYTES {
            let response = ShellAskDecision::deny("in-flight permission request is too large");
            stream
                .write_all(&serde_json::to_vec(&response).map_err(std::io::Error::other)?)
                .await?;
            stream.shutdown().await?;
            return Ok(());
        }
    }

    let decision = match serde_json::from_slice::<ShellAskWireRequest>(&bytes) {
        Ok(wire) if !wire.command.trim().is_empty() => {
            approver(ShellAskRequest {
                call_id,
                parent_command,
                command: wire.command,
                justification: wire.justification,
                workdir,
            })
            .await
        }
        Ok(_) => ShellAskDecision::deny("in-flight permission command must not be empty"),
        Err(err) => ShellAskDecision::deny(format!("invalid in-flight permission request: {err}")),
    };
    stream
        .write_all(&serde_json::to_vec(&decision).map_err(std::io::Error::other)?)
        .await?;
    stream.shutdown().await?;
    Ok(())
}

fn sanitize_shell_call_id(call_id: &str) -> String {
    let mut out = String::new();
    for ch in call_id.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "call".to_string()
    } else {
        out
    }
}

async fn terminate_shell_child(child: &mut tokio::process::Child, grace_ms: u64) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        kill_process_group(pid, libc::SIGTERM);
        if time::timeout(Duration::from_millis(grace_ms), child.wait())
            .await
            .is_ok()
        {
            return;
        }
        kill_process_group(pid, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = grace_ms;
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(unix)]
fn kill_process_group(pid: u32, signal: libc::c_int) {
    unsafe {
        let _ = libc::kill(-(pid as libc::pid_t), signal);
    }
}

fn apply_shell_environment_policy(
    command: &mut Command,
    config: &ShellSandboxConfig,
) -> Vec<String> {
    let mut preserved = BTreeMap::<String, OsString>::new();
    for (name, value) in env::vars_os() {
        let Some(name) = name.to_str() else {
            continue;
        };
        if shell_env_should_preserve(name, &config.env_allowlist) {
            preserved.insert(name.to_string(), value);
        }
    }

    command.env_clear();
    for (name, value) in &preserved {
        command.env(name, value);
    }
    preserved.into_keys().collect()
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

#[derive(Clone, Default)]
struct ShellStreamCapture {
    bytes: Arc<Mutex<Vec<u8>>>,
    truncated: Arc<AtomicBool>,
}

impl ShellStreamCapture {
    async fn append(&self, chunk: &[u8], cap: usize) {
        let mut bytes = self.bytes.lock().await;
        let keep = chunk.len().min(cap.saturating_sub(bytes.len()));
        if keep > 0 {
            bytes.extend_from_slice(&chunk[..keep]);
        }
        if keep < chunk.len() {
            self.truncated.store(true, Ordering::Relaxed);
        }
    }

    fn mark_truncated(&self) {
        self.truncated.store(true, Ordering::Relaxed);
    }

    async fn snapshot(&self) -> (Vec<u8>, bool) {
        (
            self.bytes.lock().await.clone(),
            self.truncated.load(Ordering::Relaxed),
        )
    }
}

async fn read_limited_pipe<R>(
    mut reader: Option<R>,
    cap: usize,
    capture: ShellStreamCapture,
) -> std::result::Result<(), std::io::Error>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(mut reader) = reader.take() else {
        return Ok(());
    };
    let mut buffer = vec![0u8; 8192];

    loop {
        let count = match reader.read(&mut buffer).await {
            Ok(count) => count,
            Err(err) if err.raw_os_error() == Some(libc::EIO) => break,
            Err(err) => return Err(err),
        };
        if count == 0 {
            break;
        }
        capture.append(&buffer[..count], cap).await;
    }

    Ok(())
}

async fn drain_or_abort(
    mut handle: tokio::task::JoinHandle<std::result::Result<(), std::io::Error>>,
    capture: ShellStreamCapture,
    timeout: Duration,
) -> std::result::Result<(Vec<u8>, bool), std::io::Error> {
    match time::timeout(timeout, &mut handle).await {
        Ok(joined) => {
            joined.map_err(|err| {
                std::io::Error::other(format!("shell output reader failed: {err}"))
            })??;
        }
        Err(_) => {
            handle.abort();
            capture.mark_truncated();
        }
    }
    Ok(capture.snapshot().await)
}

fn split_shell_output(
    stdout: Vec<u8>,
    stdout_truncated: bool,
    stderr: Vec<u8>,
    stderr_truncated: bool,
    output_cap: usize,
) -> (Vec<u8>, bool, Vec<u8>, bool) {
    if output_cap == 0 || stdout.len().saturating_add(stderr.len()) <= output_cap {
        return (stdout, stdout_truncated, stderr, stderr_truncated);
    }

    let stdout_floor = if output_cap >= 24 * 1024 {
        (output_cap / 3).max(8 * 1024)
    } else {
        (output_cap / 3).max(1)
    }
    .min(output_cap);
    let mut stdout_take = stdout.len().min(stdout_floor);
    let mut stderr_take = stderr.len().min(output_cap.saturating_sub(stdout_take));
    let mut remaining = output_cap.saturating_sub(stdout_take + stderr_take);
    let extra_stdout = remaining.min(stdout.len().saturating_sub(stdout_take));
    stdout_take += extra_stdout;
    remaining = remaining.saturating_sub(extra_stdout);
    let extra_stderr = remaining.min(stderr.len().saturating_sub(stderr_take));
    stderr_take += extra_stderr;

    let final_stdout_truncated = stdout_truncated || stdout_take < stdout.len();
    let final_stderr_truncated = stderr_truncated || stderr_take < stderr.len();
    (
        stdout[..stdout_take].to_vec(),
        final_stdout_truncated,
        stderr[..stderr_take].to_vec(),
        final_stderr_truncated,
    )
}
