use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

pub(crate) fn git_text<I, S>(cwd: &Path, args: I) -> std::result::Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let output = git_output(cwd, args)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(crate) fn git_output<I, S>(cwd: &Path, args: I) -> std::result::Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    git_output_allow_status(cwd, args, &[0])
}

pub(crate) fn git_output_allow_status<I, S>(
    cwd: &Path,
    args: I,
    success: &[i32],
) -> std::result::Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    git_output_vec_allow_status(
        cwd,
        args.into_iter()
            .map(|arg| arg.as_ref().to_string())
            .collect(),
        success,
    )
}

pub(crate) fn git_output_vec_allow_status(
    cwd: &Path,
    args: Vec<String>,
    success: &[i32],
) -> std::result::Result<Output, String> {
    git_output_vec_with_stdin_allow_status(cwd, args, Vec::new(), success)
}

pub(crate) fn git_output_vec_with_stdin_allow_status(
    cwd: &Path,
    args: Vec<String>,
    stdin: Vec<u8>,
    success: &[i32],
) -> std::result::Result<Output, String> {
    let mut command = Command::new("git");
    command
        .args([
            "--no-optional-locks",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotepath=false",
        ])
        .args(args)
        .current_dir(cwd);
    if !stdin.is_empty() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("git failed to start: {err}"))?;
    if !stdin.is_empty()
        && let Some(mut handle) = child.stdin.take()
    {
        handle
            .write_all(&stdin)
            .map_err(|err| format!("git stdin write failed: {err}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|err| format!("git wait failed: {err}"))?;
    let code = output.status.code().unwrap_or(-1);
    if success.contains(&code) {
        Ok(output)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("git exited with status {code}")
        } else {
            stderr
        })
    }
}

pub(crate) fn hooks_off_value() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// `true` when every non-empty line of `git add` stderr is part of the
/// "paths are ignored by one of your .gitignore files" advisory.
///
/// The advisory body is: a header line, the listed paths (one per line,
/// flush-left, no leading whitespace), and zero or more `hint: ...` lines
/// suggesting `-f` or `git config advice.addIgnoredFile false`. When that
/// is the entire stderr, the add already succeeded for every non-excluded
/// path and the non-zero exit is purely informational.
///
/// Any `fatal:` / `error:` / unknown diagnostic line, even mixed in after
/// the header, causes this to return `false` so the real error still
/// propagates through `SqueezyError::Tool`.
pub(crate) fn is_add_ignored_advisory_only(stderr: &str) -> bool {
    let mut saw_header = false;
    for raw_line in stderr.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("hint:") {
            continue;
        }
        if line == "The following paths are ignored by one of your .gitignore files:" {
            saw_header = true;
            continue;
        }
        if !saw_header {
            return false;
        }
        // After the header, accept only listed pathspecs: literal path
        // tokens with no whitespace. Anything else (e.g. `fatal: ...`,
        // `error: ...`, a stray `warning: ...`) is a real diagnostic and must
        // propagate.
        if line.contains(char::is_whitespace) {
            return false;
        }
        if line.starts_with("fatal:") || line.starts_with("error:") {
            return false;
        }
    }
    saw_header
}
