use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use squeezy_hooks::{HookContext, HookEvent, HookHandler, HookPayload, HookRegistry, HookResult};
use tracing::warn;

use crate::LoadedSkill;

/// Default number of seconds to wait for a skill hook command before
/// killing it and returning a deny result. Avoids blocking the agent
/// turn on a hook that hangs (e.g. `sleep infinity`, blocked I/O).
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 30;

/// One matcher clause inside a per-event hook block.
///
/// `matcher` is an optional tool-name filter consulted by the handler at
/// dispatch time — `None` (or the literal `"*"`) means every payload for
/// the event fires this matcher's hooks. Unknown events and unknown hook
/// kinds drop with a `tracing::warn!` rather than failing the skill load,
/// matching the broader frontmatter parsing contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillHookMatcher {
    pub matcher: Option<String>,
    pub hooks: Vec<SkillHookSpec>,
}

/// What to do when a skill hook command fails to spawn (e.g. missing `sh` on
/// Windows). Defaults to `Allow` to preserve existing behavior, but operators
/// can set `Deny` for policy-enforcement hooks where a spawn failure must not
/// silently become permissive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HookFailurePolicy {
    #[default]
    Allow,
    Deny,
}

/// One concrete hook handler declaration.
///
/// Today only the `command` kind is implemented: it shells out to the
/// declared `command` line, resolved relative to the skill's `base_dir`
/// when the path is relative. `once: true` semantics live in the handler
/// (self-skipped after the first *successful* run) so the registry stays
/// agnostic; a failed first run is retried on the next dispatch.
///
/// `kind_valid` is `false` when the spec's `type:` field was set to an
/// unsupported value. Such specs are dropped before handler registration
/// so a frontmatter block with `type: webhook` + `command: ...` does
/// not silently execute as a shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillHookSpec {
    pub command: String,
    pub once: bool,
    /// Maximum seconds to wait for the hook command before killing it
    /// and returning a deny result. Defaults to
    /// [`DEFAULT_HOOK_TIMEOUT_SECS`] when `None`.
    pub timeout_secs: Option<u64>,
    /// When `false` (fail-closed), a spawn error or `wait()` error returns
    /// a deny result instead of silently allowing execution. Defaults to
    /// `true` for backward-compatibility with the original fail-open
    /// behaviour; set `fail_open = false` in the frontmatter for enforcement
    /// hooks that must not silently pass when the interpreter is missing.
    ///
    /// **Note**: a hook that exceeds `timeout_secs` always returns deny
    /// regardless of `fail_open`, because a hung hook is an anomaly that
    /// should not silently pass in either audit or enforcement configurations.
    pub fail_open: bool,
    /// `false` when an unsupported `type:` was declared; prevents
    /// execution even if a `command:` line was also present.
    pub kind_valid: bool,
    /// Policy applied when the hook command fails to spawn (e.g. shell not in
    /// `PATH`). `Allow` (default) preserves backward compatibility. `Deny`
    /// makes spawn failures behave like a non-zero exit, preventing a missing
    /// shell from silently neutralizing a policy hook.
    pub failure_policy: HookFailurePolicy,
}

const PAYLOAD_INLINE_THRESHOLD: usize = 8 * 1024;

/// Per-process dispatch sequence number used to generate unique temp-file
/// names. A plain counter is enough: file names only need to be unique
/// within a single process, not globally.
static HOOK_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Resolve the shell program and arguments used to run a hook command
/// string on the current platform.
///
/// On Unix, returns `("/bin/sh", ["-c"])`. On Windows, walks the candidates
/// `pwsh`, `powershell`, and `cmd` in that order, returning the first
/// one found on `PATH`. Returns `None` only when every candidate is
/// absent — callers treat that as a spawn error and allow the action
/// (fail-open) with a clear diagnostic.
fn resolve_hook_shell_program() -> Option<(String, Vec<String>)> {
    #[cfg(windows)]
    {
        let candidates: &[(&str, &[&str])] = &[
            ("pwsh", &["-NoProfile", "-Command"]),
            ("powershell", &["-NoProfile", "-Command"]),
            ("cmd", &["/C"]),
        ];
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        for &(shell, args) in candidates {
            let found = std::env::split_paths(&path_var)
                .any(|dir| dir.join(format!("{shell}.exe")).exists() || dir.join(shell).exists());
            if found {
                return Some((
                    shell.to_string(),
                    args.iter().map(|s| s.to_string()).collect(),
                ));
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        Some(("/bin/sh".to_string(), vec!["-c".to_string()]))
    }
}

/// [`HookHandler`] implementation that fires a skill's declared shell
/// command when its event matches.
///
/// `event` is the variant from the skill's frontmatter; the handler
/// fast-paths-returns `HookResult::allow()` without spawning a process
/// when `ctx.event` doesn't match, so registering hooks for one event
/// stays cheap on unrelated dispatches. `matcher` (when present) is
/// matched against the `tool_name` payload field on tool-scoped events;
/// `None` means the handler fires for every payload of the event.
/// `base_dir` is the skill's filesystem root and lets the handler
/// resolve relative `command` paths the same way CC resolves
/// `${CLAUDE_PLUGIN_ROOT}`.
pub struct SkillHookHandler {
    skill_name: String,
    event: HookEvent,
    matcher: Option<String>,
    spec: SkillHookSpec,
    base_dir: PathBuf,
    /// Tracks whether a `once: true` hook is already claimed or has succeeded
    /// in this session. A failed claimed run resets the flag so it can be
    /// retried. `AtomicBool` with `AcqRel` / `Acquire` ordering is used rather
    /// than `Mutex<bool>` to close the TOCTOU gap where two concurrent
    /// dispatches could both read `false` before either writes `true`, and to
    /// avoid the silent-pass risk of a poisoned mutex.
    fired: AtomicBool,
    /// Cached shell program and arguments for this handler, populated on
    /// the first dispatch. Avoids re-walking PATH on every hook invocation
    /// and makes the resolution cost visible (one OnceLock init per handler
    /// rather than one per dispatch).
    resolved_shell: OnceLock<Option<(String, Vec<String>)>>,
}

impl SkillHookHandler {
    pub fn new(
        skill_name: String,
        event: HookEvent,
        matcher: Option<String>,
        spec: SkillHookSpec,
        base_dir: PathBuf,
    ) -> Self {
        Self {
            skill_name,
            event,
            matcher,
            spec,
            base_dir,
            fired: AtomicBool::new(false),
            resolved_shell: OnceLock::new(),
        }
    }
}

impl HookHandler for SkillHookHandler {
    fn handle(&self, ctx: &HookContext) -> HookResult {
        if ctx.event != self.event {
            return HookResult::allow();
        }

        // Match tool_name directly from the typed payload before
        // projecting to JSON, so unrelated tool dispatches pay no
        // serialization cost.
        if let Some(needle) = self.matcher.as_deref() {
            let tool_name = match &ctx.payload {
                HookPayload::PreToolUse { tool_name, .. }
                | HookPayload::PostToolUse { tool_name, .. }
                | HookPayload::PostToolUseFailure { tool_name, .. }
                | HookPayload::PostTool { tool_name, .. }
                | HookPayload::PermissionRequest { tool_name, .. }
                | HookPayload::PermissionDenied { tool_name, .. } => tool_name.as_str(),
                _ => "",
            };
            if tool_name != needle {
                return HookResult::allow();
            }
        }

        let trimmed = self.spec.command.trim();
        if trimmed.is_empty() {
            warn!(
                target: "squeezy_skills",
                skill = %self.skill_name,
                "skipping skill hook with empty command"
            );
            return HookResult::allow();
        }

        let once_claimed = if self.spec.once {
            if self
                .fired
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return HookResult::allow();
            }
            true
        } else {
            false
        };

        let shell_info = self.resolved_shell.get_or_init(resolve_hook_shell_program);
        let (shell, shell_args) = match shell_info {
            Some(pair) => (&pair.0, &pair.1),
            None => {
                warn!(
                    target: "squeezy_skills",
                    skill = %self.skill_name,
                    "skill hook failed to spawn: no suitable hook shell found on PATH"
                );
                if once_claimed {
                    self.fired.store(false, Ordering::Release);
                }
                return if self.spec.fail_open
                    && self.spec.failure_policy == HookFailurePolicy::Allow
                {
                    HookResult::allow()
                } else {
                    HookResult::deny(format!("skill `{}` hook shell not found", self.skill_name))
                };
            }
        };

        let payload = ctx.payload_json().to_string();
        let seq = HOOK_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let payload_file_path = if payload.len() > PAYLOAD_INLINE_THRESHOLD {
            let path = std::env::temp_dir()
                .join(format!("squeezy_hook_{}_{seq}.json", std::process::id()));
            match fs::write(&path, &payload) {
                Ok(()) => Some(path),
                Err(error) => {
                    warn!(
                        target: "squeezy_skills",
                        skill = %self.skill_name,
                        error = %error,
                        "failed to write hook payload temp file; falling back to env-only delivery"
                    );
                    None
                }
            }
        } else {
            None
        };
        let cleanup = |path: Option<&Path>| {
            if let Some(path) = path {
                let _ = fs::remove_file(path);
            }
        };

        let mut command = Command::new(shell);
        for arg in shell_args {
            command.arg(arg);
        }

        // Put the child in its own process group so a timeout signal reaches
        // grandchildren spawned by the hook shell script, not just the shell.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        command
            .arg(trimmed)
            .current_dir(&self.base_dir)
            .env("SQUEEZY_SKILL_DIR", &self.base_dir)
            .env("SQUEEZY_SKILL_NAME", &self.skill_name)
            // Redirect subprocess stdio to /dev/null so hook scripts cannot
            // corrupt the TUI or write to the agent's streams.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        // Deliver payload via file for large payloads and via env var for
        // small ones. Explicitly clear the alternate variable so stale
        // inherited values cannot confuse hook scripts.
        if let Some(ref path) = payload_file_path {
            command
                .env("SQUEEZY_HOOK_PAYLOAD_FILE", path)
                .env_remove("SQUEEZY_HOOK_PAYLOAD");
        } else {
            command
                .env("SQUEEZY_HOOK_PAYLOAD", &payload)
                .env_remove("SQUEEZY_HOOK_PAYLOAD_FILE");
        }

        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                warn!(
                    target: "squeezy_skills",
                    skill = %self.skill_name,
                    shell = %shell,
                    error = %error,
                    "skill hook failed to spawn"
                );
                cleanup(payload_file_path.as_deref());
                if once_claimed {
                    self.fired.store(false, Ordering::Release);
                }
                return if self.spec.fail_open
                    && self.spec.failure_policy == HookFailurePolicy::Allow
                {
                    HookResult::allow()
                } else {
                    HookResult::deny(format!(
                        "skill `{}` hook failed to spawn: {}",
                        self.skill_name, error
                    ))
                };
            }
        };

        // Capture PID before wrapping child in Arc. On Unix, used to send
        // SIGKILL to the process group on timeout so all grandchildren are
        // terminated; elided on non-Unix to avoid an unused-variable warning.
        #[cfg(unix)]
        let child_pid = child.id();

        // Wrap child in Arc<Mutex<Option<...>>> so the main thread can call
        // `kill()` on timeout without a blocking wait-for-lock: the wait
        // thread takes the child out of the Option before calling `wait()`.
        let child_arc = Arc::new(Mutex::new(Some(child)));
        let child_for_thread = Arc::clone(&child_arc);

        let timeout =
            Duration::from_secs(self.spec.timeout_secs.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS));
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = child_for_thread
                .lock()
                .ok()
                .and_then(|mut guard| guard.take())
                .map(|mut child| child.wait());
            if let Some(result) = result {
                let _ = tx.send(result);
            }
        });

        match rx.recv_timeout(timeout) {
            Ok(Ok(status)) if status.success() => {
                cleanup(payload_file_path.as_deref());
                HookResult::allow()
            }
            Ok(Ok(status)) => {
                cleanup(payload_file_path.as_deref());
                let code = status.code();
                let detail = match code {
                    Some(126) => format!(
                        "skill `{}` hook: command not executable (exit 126)",
                        self.skill_name
                    ),
                    Some(127) => format!(
                        "skill `{}` hook: interpreter or command not found (exit 127)",
                        self.skill_name
                    ),
                    _ => format!("skill `{}` hook denied the action", self.skill_name),
                };
                warn!(
                    target: "squeezy_skills",
                    skill = %self.skill_name,
                    code = ?code,
                    "skill hook exited non-zero"
                );
                if once_claimed {
                    self.fired.store(false, Ordering::Release);
                }
                HookResult::deny(detail)
            }
            Ok(Err(error)) => {
                cleanup(payload_file_path.as_deref());
                warn!(
                    target: "squeezy_skills",
                    skill = %self.skill_name,
                    error = %error,
                    "skill hook wait() error"
                );
                if once_claimed {
                    self.fired.store(false, Ordering::Release);
                }
                if self.spec.fail_open && self.spec.failure_policy == HookFailurePolicy::Allow {
                    HookResult::allow()
                } else {
                    HookResult::deny(format!(
                        "skill `{}` hook wait failed: {}",
                        self.skill_name, error
                    ))
                }
            }
            Err(_timeout_expired) => {
                cleanup(payload_file_path.as_deref());
                if let Ok(mut guard) = child_arc.lock()
                    && let Some(child) = guard.as_mut()
                {
                    let _ = child.kill();
                }
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(child_pid as libc::pid_t), libc::SIGKILL);
                }
                warn!(
                    target: "squeezy_skills",
                    skill = %self.skill_name,
                    timeout_secs = self.spec.timeout_secs.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS),
                    "skill hook timed out"
                );
                if once_claimed {
                    self.fired.store(false, Ordering::Release);
                }
                HookResult::deny(format!(
                    "skill `{}` hook timed out after {}s",
                    self.skill_name,
                    self.spec.timeout_secs.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS)
                ))
            }
        }
    }
}

/// Register every hook declared in a [`LoadedSkill`]'s frontmatter
/// against the given [`HookRegistry`].
///
/// Specs with `kind_valid = false` (unsupported `type:` in frontmatter)
/// are silently dropped so they cannot execute as shell commands.
///
/// Handlers are registered via [`HookRegistry::register_for_event`] so
/// the registry can dispatch in O(matching handlers) rather than
/// O(total handlers). Returns the number of handlers installed so
/// callers can log the activation count alongside the skill name.
pub fn register_skill_hooks(skill: &LoadedSkill, registry: &mut HookRegistry) -> usize {
    let mut installed = 0;
    for (event, matchers) in &skill.hooks {
        for matcher in matchers {
            for spec in &matcher.hooks {
                if !spec.kind_valid {
                    continue;
                }
                registry.register_for_event(
                    *event,
                    Box::new(SkillHookHandler::new(
                        skill.summary.name.clone(),
                        *event,
                        matcher.matcher.clone(),
                        spec.clone(),
                        skill.base_dir.clone(),
                    )),
                );
                installed += 1;
            }
        }
    }
    if installed > 0 {
        tracing::info!(
            target: "squeezy_skills",
            skill = %skill.summary.name,
            source = %skill.base_dir.display(),
            installed,
            "registered skill frontmatter hooks"
        );
        // Emit a per-handler snapshot at DEBUG level so session logs
        // include the full hook registry for trusted local debugging.
        for (event, matchers) in &skill.hooks {
            for matcher_spec in matchers {
                for spec in &matcher_spec.hooks {
                    if !spec.kind_valid {
                        continue;
                    }
                    tracing::debug!(
                        target: "squeezy_skills",
                        skill = %skill.summary.name,
                        event = ?event,
                        matcher = ?matcher_spec.matcher,
                        command_path = %spec.command,
                        once = spec.once,
                        source = %skill.base_dir.display(),
                        "hook handler registered"
                    );
                }
            }
        }
    }
    installed
}

#[cfg(test)]
#[path = "hooks_tests.rs"]
mod tests;
