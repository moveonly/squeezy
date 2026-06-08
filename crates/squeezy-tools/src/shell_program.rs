//! Platform-correct shell program selection for the cross-platform shell
//! sandbox direct/external execution paths. The macOS sandbox-exec and Linux
//! direct-syscalls backends keep their hardcoded `sh -lc` invocation inside
//! their own `cfg(target_os = ...)` blocks; this module covers everything
//! else.

#[cfg(windows)]
use std::sync::OnceLock;

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
    /// On Windows the default resolution result is cached per process via a
    /// `OnceLock` so repeated calls (e.g. in shell-heavy sessions) do not
    /// probe `which` on every plan.
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

    fn unix_sh(command: &str) -> Self {
        Self {
            program: "sh".to_string(),
            args: vec!["-lc".to_string(), command.to_string()],
            display_name: "sh".to_string(),
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
        let (args, display_name) =
            if lowered.ends_with("pwsh.exe") || lowered.ends_with("powershell.exe") {
                (
                    vec![
                        "-NoLogo".to_string(),
                        "-NoProfile".to_string(),
                        "-Command".to_string(),
                        command.to_string(),
                    ],
                    std::path::Path::new(path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(path)
                        .to_string(),
                )
            } else if lowered.ends_with("cmd.exe") {
                (
                    vec![
                        "/D".to_string(),
                        "/S".to_string(),
                        "/C".to_string(),
                        command.to_string(),
                    ],
                    "cmd.exe".to_string(),
                )
            } else {
                let name = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path)
                    .to_string();
                (vec!["-lc".to_string(), command.to_string()], name)
            };
        Self {
            program: path.to_string(),
            args,
            display_name,
        }
    }

    #[cfg(windows)]
    fn windows_default(command: &str) -> Self {
        // Resolve and cache the default shell binary per process.  `which`
        // probes PATH on every call; caching the resolved path avoids repeated
        // filesystem lookups in shell-heavy sessions while still picking up a
        // newly installed shell after a restart.
        static CACHED: OnceLock<(String, &'static str)> = OnceLock::new();
        let (program, display_name) = CACHED.get_or_init(|| {
            if let Ok(pwsh) = which::which("pwsh") {
                return (pwsh.to_string_lossy().into_owned(), "pwsh");
            }
            if let Ok(powershell) = which::which("powershell") {
                return (powershell.to_string_lossy().into_owned(), "powershell");
            }
            ("cmd.exe".to_string(), "cmd.exe")
        });
        let args = if display_name.starts_with("pwsh") || display_name.starts_with("powershell") {
            vec![
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                command.to_string(),
            ]
        } else {
            vec![
                "/D".to_string(),
                "/S".to_string(),
                "/C".to_string(),
                command.to_string(),
            ]
        };
        Self {
            program: program.clone(),
            args,
            display_name: display_name.to_string(),
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
}
