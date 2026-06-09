//! Platform-correct shell program selection for the cross-platform shell
//! sandbox direct/external execution paths. The macOS sandbox-exec and Linux
//! direct-syscalls backends keep their hardcoded `sh -lc` invocation inside
//! their own `cfg(target_os = ...)` blocks; this module covers everything
//! else.

use std::sync::Mutex;
#[cfg(windows)]
use std::sync::OnceLock;

/// User-facing description of the shell that `!cmd` / `!!cmd` will launch.
///
/// Reads `SQUEEZY_SHELL` exactly like [`ShellProgram::for_command`] does — an
/// unset or empty value falls back to the platform default label. Both the
/// TUI `/terminal` diagnostic and the agent's `[shell: ...]` failure hint
/// call this so they agree on what they print, including the empty-string
/// and non-UTF-8 cases (handled lossily, not silently dropped).
pub fn effective_shell_label() -> String {
    if let Some(value) = std::env::var_os("SQUEEZY_SHELL")
        && !value.is_empty()
    {
        return value.to_string_lossy().into_owned();
    }
    default_shell_label().to_string()
}

#[cfg(unix)]
const fn default_shell_label() -> &'static str {
    "sh -lc (default)"
}

#[cfg(windows)]
const fn default_shell_label() -> &'static str {
    "pwsh/powershell/cmd auto-select (default)"
}

#[cfg(not(any(unix, windows)))]
const fn default_shell_label() -> &'static str {
    "sh -lc (default)"
}

#[derive(Debug, Clone)]
pub(crate) struct ShellProgram {
    pub program: String,
    pub args: Vec<String>,
    /// Short display name for diagnostics (e.g. `"pwsh"`, `"powershell"`,
    /// `"cmd.exe"`, `"gitbash"`, `"sh"`, or the custom path basename).
    /// Populated on all platforms; consumed by Windows sandbox plan metadata
    /// and cached shell resolution. Dead-code lint is suppressed because the
    /// field is read only inside `#[cfg(windows)]` blocks.
    #[allow(dead_code)]
    pub display_name: String,
}

/// Cached result of default-shell resolution. On Windows, calling
/// `which::which` on every `for_command` invocation performs process-level
/// PATH walks; caching the resolved binary amortises that cost across all
/// shell tool calls in the same process.
///
/// The cache is only populated when `SQUEEZY_SHELL` is not set (the default
/// resolution path). Override paths (`SQUEEZY_SHELL=…`) are not cached
/// because they are already fast (direct path or a single `which` lookup).
/// The entry is discarded when `SQUEEZY_SHELL` transitions from unset to set.
#[derive(Debug, Clone)]
struct CachedShellBase {
    /// Resolved program path (e.g. `C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe`).
    program: String,
    /// Argument prefix that precedes the command string (e.g.
    /// `["-NoLogo", "-NoProfile", "-Command"]` for PowerShell).
    arg_prefix: Vec<String>,
    display_name: String,
}

static SHELL_BASE_CACHE: Mutex<Option<CachedShellBase>> = Mutex::new(None);
#[cfg(windows)]
static WINDOWS_SHELL_BINARY: OnceLock<String> = OnceLock::new();

impl ShellProgram {
    /// Resolve the shell program + arguments to run `command`.
    ///
    /// Honors `SQUEEZY_SHELL` first:
    /// - `gitbash` — search `PROGRAMFILES`-style locations for Git Bash.
    ///   Note: `SQUEEZY_SHELL=gitbash` is a compatibility and CI testing
    ///   choice, not the production default. Production Windows defaults to
    ///   `pwsh` → `powershell` → `cmd.exe`.
    /// - any other value — treat as the absolute path of the shell binary.
    ///
    /// Without an override:
    /// - Unix: `sh -lc <command>` (POSIX shell, login mode).
    /// - Windows: `pwsh.exe` → `powershell.exe` → `cmd.exe`, in that order,
    ///   resolved via `which::which`. The shell call follows each shell's
    ///   convention (`-NoLogo -NoProfile -Command` for PowerShell variants,
    ///   `/D /S /C` for cmd).
    ///
    /// Caching: the cache is consulted only when `SQUEEZY_SHELL` is unset.
    /// Setting `SQUEEZY_SHELL` temporarily takes a non-cached path (the
    /// override is resolved per-call), but unsetting it again returns the
    /// previously cached default. `PATH` changes during the process
    /// lifetime are not detected; the cached `which::which` result is
    /// reused for the rest of the process. Override and git-bash paths
    /// bypass the cache entirely.
    pub(crate) fn for_command(command: &str) -> Self {
        // Try the per-process cache when no override is configured. The
        // SQUEEZY_SHELL override paths are cheap and command-specific, so
        // they bypass the cache.
        if std::env::var_os("SQUEEZY_SHELL").is_none() {
            // Tentative read: hold the lock only long enough to clone the
            // cached entry. `which::which` (a potentially blocking PATH
            // walk on Windows) runs outside the critical section so other
            // tokio futures contending on shell tool calls don't queue
            // behind a first-time resolution.
            if let Ok(guard) = SHELL_BASE_CACHE.lock()
                && let Some(ref cached) = *guard
            {
                let mut args = cached.arg_prefix.clone();
                args.push(command.to_string());
                return Self {
                    program: cached.program.clone(),
                    args,
                    display_name: cached.display_name.clone(),
                };
            }
            let resolved = Self::resolve_default(command);
            // The command is always the last argument; store the prefix.
            let arg_prefix: Vec<String> =
                resolved.args[..resolved.args.len().saturating_sub(1)].to_vec();
            // Re-acquire the lock to publish. If a concurrent caller
            // already populated the cache, leave their value in place —
            // both will be equivalent under the "no SQUEEZY_SHELL, same
            // PATH" precondition we just used.
            if let Ok(mut guard) = SHELL_BASE_CACHE.lock()
                && guard.is_none()
            {
                *guard = Some(CachedShellBase {
                    program: resolved.program.clone(),
                    arg_prefix,
                    display_name: resolved.display_name.clone(),
                });
            }
            return resolved;
        }

        if let Ok(custom) = std::env::var("SQUEEZY_SHELL") {
            return Self::resolve_override(&custom, command);
        }
        Self::resolve_default(command)
    }

    /// Resolve without consulting the cache. Called on the first cache miss
    /// or when the cache lock is unavailable.
    fn resolve_default(command: &str) -> Self {
        #[cfg(unix)]
        {
            Self::unix_sh(command)
        }
        #[cfg(windows)]
        {
            Self::windows_default_cached(command)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self::unix_sh(command)
        }
    }

    /// Validate the `SQUEEZY_SHELL` override at plan-construction time and
    /// return a user-facing error before any spawn attempt if the override
    /// cannot be satisfied. Currently covers:
    /// - `SQUEEZY_SHELL=gitbash` on Windows when Git Bash is not found — a
    ///   clear error is better than falling back to `sh -lc` (likely absent)
    ///   and getting a confusing spawn failure later.
    pub(crate) fn validate_override() -> Result<(), String> {
        let Ok(spec) = std::env::var("SQUEEZY_SHELL") else {
            return Ok(());
        };
        match spec.as_str() {
            "gitbash" => {
                #[cfg(windows)]
                if Self::git_bash("echo").is_none() {
                    return Err(
                        "SQUEEZY_SHELL=gitbash: Git Bash not found at well-known Windows \
                         locations (C:\\Program Files\\Git\\bin\\bash.exe, \
                         C:\\Program Files (x86)\\Git\\bin\\bash.exe) and \
                         SQUEEZY_GIT_BASH_PATH is not set. \
                         Install Git for Windows or point SQUEEZY_GIT_BASH_PATH at \
                         the correct bash.exe path."
                            .to_string(),
                    );
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    #[cfg(windows)]
    pub(crate) fn for_windows_restricted_command(command: &str) -> Self {
        if let Ok(custom) = std::env::var("SQUEEZY_SHELL") {
            return Self::resolve_override(&custom, command);
        }
        Self::windows_cmd(command)
    }

    fn unix_sh(command: &str) -> Self {
        Self {
            program: "sh".to_string(),
            args: vec!["-lc".to_string(), command.to_string()],
            display_name: "sh".to_string(),
        }
    }

    fn resolve_override(spec: &str, command: &str) -> Self {
        match spec {
            "gitbash" => {
                // `git_bash` is only available on unix and windows; on other
                // targets, treat the request the same as the unix sh fallback.
                #[cfg(any(unix, target_os = "windows"))]
                {
                    Self::git_bash(command).unwrap_or_else(|| Self::unix_sh(command))
                }
                #[cfg(not(any(unix, target_os = "windows")))]
                {
                    Self::unix_sh(command)
                }
            }
            _ => Self::custom_path(spec, command),
        }
    }

    fn custom_path(path: &str, command: &str) -> Self {
        // Best-effort heuristic: if the user pointed at a known shell, pick
        // its argument shape. Recognises both bare names (`pwsh`, `powershell`,
        // `cmd`) and full `.exe` paths so that `SQUEEZY_SHELL=pwsh` gets the
        // correct `-NoLogo -NoProfile -Command` argument shape instead of the
        // POSIX `-lc` fallback.
        let lowered = path.to_ascii_lowercase();
        let display_name = std::path::Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path)
            .to_string();
        let args = if lowered == "pwsh"
            || lowered == "powershell"
            || lowered.ends_with("pwsh.exe")
            || lowered.ends_with("powershell.exe")
        {
            vec![
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                command.to_string(),
            ]
        } else if lowered == "cmd" || lowered.ends_with("cmd.exe") {
            vec![
                "/D".to_string(),
                "/S".to_string(),
                "/C".to_string(),
                command.to_string(),
            ]
        } else {
            vec!["-lc".to_string(), command.to_string()]
        };
        Self {
            program: path.to_string(),
            args,
            display_name,
        }
    }

    /// Cached version of `windows_default`: resolves the binary path once via
    /// PATH probing and reuses it for subsequent calls in the same session.
    #[cfg(windows)]
    fn windows_default_cached(command: &str) -> Self {
        let binary = WINDOWS_SHELL_BINARY.get_or_init(Self::resolve_windows_shell_binary);
        Self::windows_shell_with_binary(binary, command)
    }

    /// Probe PATH for the best available Windows shell binary.
    #[cfg(windows)]
    fn resolve_windows_shell_binary() -> String {
        if let Ok(pwsh) = which::which("pwsh") {
            return pwsh.to_string_lossy().into_owned();
        }
        if let Ok(powershell) = which::which("powershell") {
            return powershell.to_string_lossy().into_owned();
        }
        "cmd.exe".to_string()
    }

    /// Build a `ShellProgram` for the given cached binary path and command.
    #[cfg(windows)]
    fn windows_shell_with_binary(binary: &str, command: &str) -> Self {
        let lower = binary.to_ascii_lowercase();
        let display_name = std::path::Path::new(binary)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(binary)
            .to_string();
        if lower == "pwsh"
            || lower == "powershell"
            || lower.ends_with("pwsh.exe")
            || lower.ends_with("powershell.exe")
        {
            Self {
                program: binary.to_string(),
                args: vec![
                    "-NoLogo".to_string(),
                    "-NoProfile".to_string(),
                    "-Command".to_string(),
                    command.to_string(),
                ],
                display_name,
            }
        } else {
            Self {
                program: binary.to_string(),
                args: vec![
                    "/D".to_string(),
                    "/S".to_string(),
                    "/C".to_string(),
                    command.to_string(),
                ],
                display_name,
            }
        }
    }

    /// Build a cmd.exe invocation for the Windows restricted-token sandbox.
    #[cfg(windows)]
    fn windows_cmd(command: &str) -> Self {
        Self {
            program: "cmd.exe".to_string(),
            args: vec![
                "/D".to_string(),
                "/S".to_string(),
                "/C".to_string(),
                command.to_string(),
            ],
            display_name: "cmd.exe".to_string(),
        }
    }

    fn git_bash(command: &str) -> Option<Self> {
        if let Ok(path) = std::env::var("SQUEEZY_GIT_BASH_PATH")
            && std::path::Path::new(&path).is_file()
        {
            return Some(Self {
                program: path,
                args: vec!["-lc".to_string(), command.to_string()],
                display_name: "gitbash".to_string(),
            });
        }
        #[cfg(windows)]
        {
            for candidate in [
                r"C:\Program Files\Git\bin\bash.exe",
                r"C:\Program Files (x86)\Git\bin\bash.exe",
            ] {
                if std::path::Path::new(candidate).is_file() {
                    return Some(Self {
                        program: candidate.to_string(),
                        args: vec!["-lc".to_string(), command.to_string()],
                        display_name: "gitbash".to_string(),
                    });
                }
            }
        }
        // On Unix, any `bash` found via PATH is acceptable. On Windows,
        // `which::which("bash")` might resolve WSL or Cygwin bash, which is
        // not Git Bash and uses a different dialect. Skip the PATH search on
        // Windows to prevent `validate_override` from silently accepting a
        // non-Git-Bash binary.
        #[cfg(not(windows))]
        if let Ok(bash) = which::which("bash") {
            return Some(Self {
                program: bash.to_string_lossy().into_owned(),
                args: vec!["-lc".to_string(), command.to_string()],
                display_name: "gitbash".to_string(),
            });
        }
        let _ = command;
        None
    }

    #[cfg(test)]
    pub(crate) fn args_for_override(spec: &str, command: &str) -> Vec<String> {
        Self::resolve_override(spec, command).args
    }
}

#[cfg(test)]
#[path = "shell_program_tests.rs"]
mod tests;
