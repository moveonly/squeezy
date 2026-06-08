//! Platform-correct shell program selection for the cross-platform shell
//! sandbox direct/external execution paths. The macOS sandbox-exec and Linux
//! direct-syscalls backends keep their hardcoded `sh -lc` invocation inside
//! their own `cfg(target_os = ...)` blocks; this module covers everything
//! else.

use std::sync::Mutex;

#[derive(Debug, Clone)]
pub(crate) struct ShellProgram {
    pub program: String,
    pub args: Vec<String>,
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
}

static SHELL_BASE_CACHE: Mutex<Option<CachedShellBase>> = Mutex::new(None);

impl ShellProgram {
    /// Resolve the shell program + arguments to run `command`.
    ///
    /// Honors `SQUEEZY_SHELL` first:
    /// - `gitbash` — search `PROGRAMFILES`-style locations for Git Bash.
    /// - any other value — treat as the absolute path of the shell binary.
    ///
    /// Without an override:
    /// - Unix: `sh -lc <command>` (POSIX shell, login mode).
    /// - Windows: `pwsh.exe` → `powershell.exe` → `cmd.exe`, in that order,
    ///   resolved via `which::which`. The shell call follows each shell's
    ///   convention (`-NoLogo -NoProfile -Command` for PowerShell variants,
    ///   `/D /S /C` for cmd).
    ///
    /// The resolved shell binary is cached for the process lifetime and
    /// invalidated when `SQUEEZY_SHELL` changes. Only the default (no
    /// override) shell path is cached; override and git-bash paths bypass
    /// the cache because they involve per-call env-var reads of their own.
    /// This avoids repeated `which::which` PATH walks on Windows for every
    /// shell tool call.
    pub(crate) fn for_command(command: &str) -> Self {
        // Try the per-process cache when no override is configured. The
        // SQUEEZY_SHELL override paths are cheap and command-specific, so
        // they bypass the cache. If the lock is poisoned we fall through to
        // the uncached resolution path below.
        if std::env::var_os("SQUEEZY_SHELL").is_none()
            && let Ok(mut guard) = SHELL_BASE_CACHE.lock()
        {
            if let Some(ref cached) = *guard {
                let mut args = cached.arg_prefix.clone();
                args.push(command.to_string());
                return Self {
                    program: cached.program.clone(),
                    args,
                };
            }
            // Cache miss: resolve the default shell and store the result.
            let resolved = Self::resolve_default(command);
            // The command is always the last argument; store the prefix.
            let arg_prefix: Vec<String> =
                resolved.args[..resolved.args.len().saturating_sub(1)].to_vec();
            *guard = Some(CachedShellBase {
                program: resolved.program.clone(),
                arg_prefix,
            });
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
            Self::windows_default(command)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self::unix_sh(command)
        }
    }

    fn unix_sh(command: &str) -> Self {
        Self {
            program: "sh".to_string(),
            args: vec!["-lc".to_string(), command.to_string()],
        }
    }

    fn resolve_override(spec: &str, command: &str) -> Self {
        match spec {
            "gitbash" => Self::git_bash(command).unwrap_or_else(|| Self::unix_sh(command)),
            _ => Self::custom_path(spec, command),
        }
    }

    fn custom_path(path: &str, command: &str) -> Self {
        // Best-effort heuristic: if the user pointed at a known shell, pick
        // its argument shape. Otherwise default to `-lc` which most POSIX
        // shells understand.
        let lowered = path.to_ascii_lowercase();
        let args = if lowered.ends_with("pwsh.exe") || lowered.ends_with("powershell.exe") {
            vec![
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                command.to_string(),
            ]
        } else if lowered.ends_with("cmd.exe") {
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
        }
    }

    #[cfg(windows)]
    fn windows_default(command: &str) -> Self {
        if let Ok(pwsh) = which::which("pwsh") {
            return Self {
                program: pwsh.to_string_lossy().into_owned(),
                args: vec![
                    "-NoLogo".to_string(),
                    "-NoProfile".to_string(),
                    "-Command".to_string(),
                    command.to_string(),
                ],
            };
        }
        if let Ok(powershell) = which::which("powershell") {
            return Self {
                program: powershell.to_string_lossy().into_owned(),
                args: vec![
                    "-NoLogo".to_string(),
                    "-NoProfile".to_string(),
                    "-Command".to_string(),
                    command.to_string(),
                ],
            };
        }
        Self {
            program: "cmd.exe".to_string(),
            args: vec![
                "/D".to_string(),
                "/S".to_string(),
                "/C".to_string(),
                command.to_string(),
            ],
        }
    }

    /// Reset the per-process shell cache. Only available in test builds; call
    /// this in test setup when the expected default shell may differ between
    /// tests (e.g. after a PATH change or before testing the
    /// `pwsh → powershell → cmd.exe` fallback chain).
    #[cfg(test)]
    pub(crate) fn reset_shell_cache() {
        if let Ok(mut guard) = SHELL_BASE_CACHE.lock() {
            *guard = None;
        }
    }

    #[cfg(any(unix, target_os = "windows"))]
    fn git_bash(command: &str) -> Option<Self> {
        if let Ok(path) = std::env::var("SQUEEZY_GIT_BASH_PATH")
            && std::path::Path::new(&path).is_file()
        {
            return Some(Self {
                program: path,
                args: vec!["-lc".to_string(), command.to_string()],
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
                    });
                }
            }
        }
        if let Ok(bash) = which::which("bash") {
            return Some(Self {
                program: bash.to_string_lossy().into_owned(),
                args: vec!["-lc".to_string(), command.to_string()],
            });
        }
        let _ = command;
        None
    }
}
