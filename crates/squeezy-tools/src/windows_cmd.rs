//! Minimal Windows shell command safety classifier. PowerShell and cmd.exe
//! syntax is not understood by the tree-sitter-bash parser, so dangerous
//! Windows commands would otherwise be reported as `Shell` + `dynamic =
//! true` (high risk, not destructive) which is too permissive for things
//! like `Remove-Item -Recurse -Force` or `del /S /F /Q`. This module
//! pattern-matches a known set of destructive shapes so they escalate to
//! `Destructive` + `Critical`. Unknown commands keep the conservative
//! `Shell` + `dynamic = true` fallback.

/// True when `segment` is recognised as a destructive Windows shell
/// command. The matching is intentionally conservative: a benign command must
/// never trigger. The check covers PowerShell cmdlets (including aliases and
/// unordered / abbreviated parameters) and cmd.exe destructive forms.
pub(crate) fn is_destructive_windows_segment(segment: &str) -> bool {
    let lower = segment.to_ascii_lowercase();
    // Pre-tokenise once for both PowerShell and cmd.exe checks.
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    let first = tokens.first().copied().unwrap_or("");

    let powershell_command = tokens
        .iter()
        .copied()
        .find(|token| *token != "&")
        .unwrap_or("");
    let command_name = powershell_command
        .trim_matches(|ch| ch == '"' || ch == '\'')
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(powershell_command);

    // Remove-Item / ri (PowerShell).
    // Destructive when any of:
    //   - valid recurse + force parameters are present (any order)
    //   - -Confirm:$false suppresses the safety prompt
    //
    // The `ri` check matches the built-in PowerShell alias; `rm` / `rmdir`
    // are already flagged by the POSIX destructive-verb list in
    // `destructive_shell_segment_reason`.
    let is_remove_item = command_name == "remove-item" || command_name == "ri";
    if is_remove_item {
        if (powershell_has_recurse_flag(&tokens) && powershell_has_force_flag(&tokens))
            || lower.contains("-confirm:$false")
            || lower.contains("-confirm=false")
        {
            return true;
        }
    }
    if command_name == "invoke-expression" || command_name == "iex" {
        return true;
    }

    // Other unambiguously destructive PowerShell cmdlets.
    if [
        "set-executionpolicy",
        "new-localuser",
        "remove-localuser",
        "disable-localuser",
        "clear-recyclebin",
        "format-volume",
        "stop-computer",
        "restart-computer",
        "remove-service",
        "unregister-scheduledtask",
    ]
    .contains(&command_name)
    {
        return true;
    }
    if command_name == "start-process" && tokens.contains(&"-verb") {
        let has_runas = tokens.contains(&"runas");
        let launches_shell = tokens.iter().any(|t| {
            matches!(
                *t,
                "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" | "cmd" | "cmd.exe"
            )
        });
        if has_runas && launches_shell {
            return true;
        }
    }

    // cmd.exe destructive commands.
    // Tokenise to avoid matching substrings inside paths.
    let flag_matches = |flag: &str| tokens.contains(&flag);

    match first {
        // `/S` triggers recursive deletion. `/Q /F` together suppress
        // confirmation and force-delete read-only files; even without `/S`
        // that is a non-interactive, hard-to-reverse destructive operation.
        "del" | "erase" => {
            return flag_matches("/s") || (flag_matches("/q") && flag_matches("/f"));
        }
        "rd" | "rmdir" => return flag_matches("/s"),
        "format" | "diskpart" => return true,
        "vssadmin" => return flag_matches("delete"),
        "bcdedit" => {
            return flag_matches("/delete") || flag_matches("/deletevalue");
        }
        "reg" => return flag_matches("delete"),
        "cipher" => return flag_matches("/w"),
        // Service deletion via sc.exe / net stop+delete
        "sc" | "sc.exe" => return flag_matches("delete"),
        // Scheduled task deletion
        "schtasks" | "schtasks.exe" => return flag_matches("/delete"),
        // Shutdown / restart from cmd
        "shutdown" => {
            return flag_matches("/s")
                || flag_matches("/r")
                || flag_matches("/h")
                || flag_matches("-s")
                || flag_matches("-r");
        }
        _ => {}
    }

    false
}

/// True when the token list contains a `-Recurse` flag or an unambiguous
/// PowerShell abbreviation of it (`-r`, `-re`, `-rec`, …).
fn powershell_has_recurse_flag(tokens: &[&str]) -> bool {
    tokens.iter().skip(1).any(|tok| {
        let t = tok.to_ascii_lowercase();
        // Named-parameter-with-value forms: `-Recurse:$true`, `-Recurse:true`
        if t.starts_with("-recurse") {
            return true;
        }
        // Unambiguous prefix abbreviations of `-Recurse` that don't collide
        // with other common Remove-Item parameters. `-r` alone maps only to
        // Recurse in the Remove-Item parameter set.
        matches!(
            t.as_str(),
            "-r" | "-re" | "-rec" | "-recu" | "-recur" | "-recurs"
        )
    })
}

/// True when the token list contains a `-Force` flag or an unambiguous
/// PowerShell abbreviation of it. `-f` alone is intentionally excluded: in
/// Remove-Item's parameter set, `-f` is ambiguous between `-Force` and
/// `-Filter` and PowerShell itself would reject it with an ambiguity error.
/// `-fo` is the first unambiguous prefix that resolves only to `-Force`.
fn powershell_has_force_flag(tokens: &[&str]) -> bool {
    tokens.iter().skip(1).any(|tok| {
        let t = tok.to_ascii_lowercase();
        if t.starts_with("-force") {
            return true;
        }
        matches!(t.as_str(), "-fo" | "-for" | "-forc")
    })
}

#[cfg(test)]
#[path = "windows_cmd_tests.rs"]
mod tests;
