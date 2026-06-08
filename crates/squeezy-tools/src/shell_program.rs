//! Platform-correct shell program selection for the cross-platform shell
//! sandbox direct/external execution paths. The macOS sandbox-exec and Linux
//! direct-syscalls backends keep their hardcoded `sh -lc` invocation inside
//! their own `cfg(target_os = ...)` blocks; this module covers everything
//! else.

#[derive(Debug, Clone)]
pub(crate) struct ShellProgram {
    pub program: String,
    pub args: Vec<String>,
}

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
    pub(crate) fn for_command(command: &str) -> Self {
        if let Ok(custom) = std::env::var("SQUEEZY_SHELL") {
            return Self::resolve_override(&custom, command);
        }
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
            // Unsupported OS — fall through to a POSIX-shell invocation and
            // let `Command::spawn` produce the appropriate error.
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
        // its argument shape. Recognises both bare names (`pwsh`, `powershell`,
        // `cmd`) and full `.exe` paths so that `SQUEEZY_SHELL=pwsh` gets the
        // correct `-NoLogo -NoProfile -Command` argument shape instead of the
        // POSIX `-lc` fallback.
        let lowered = path.to_ascii_lowercase();
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

    #[cfg(test)]
    pub(crate) fn args_for_override(spec: &str, command: &str) -> Vec<String> {
        Self::resolve_override(spec, command).args
    }
}

#[cfg(test)]
#[path = "shell_program_tests.rs"]
mod tests;
