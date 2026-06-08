//! Minimal Windows shell command safety classifier. PowerShell and cmd.exe
//! syntax is not understood by the tree-sitter-bash parser, so dangerous
//! Windows commands would otherwise be reported as `Shell` + `dynamic =
//! true` (high risk, not destructive) which is too permissive for things
//! like `Remove-Item -Recurse -Force` or `del /S /F /Q`. This module
//! pattern-matches a short list of unambiguous destructive shapes so they
//! escalate to `Destructive` + `Critical`. Unknown commands keep the
//! conservative `Shell` + `dynamic = true` fallback.

/// True when `segment` is recognised as a destructive Windows shell
/// command. The matching is intentionally narrow: a benign command must
/// never trigger.
pub(crate) fn is_destructive_windows_segment(segment: &str) -> bool {
    let lower = segment.to_ascii_lowercase();

    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let flag_matches = |flag: &str| tokens.iter().any(|t| t.eq_ignore_ascii_case(flag));
    let has_recursive_force =
        (flag_matches("-recurse") || flag_matches("-r")) && flag_matches("-force");

    // PowerShell cmdlets / aliases where the dangerous shape is unambiguous
    // in the raw text. Each needle is a contiguous substring that does not
    // appear inside benign commands.
    //
    // `ri` is the built-in alias for `Remove-Item`.  We only flag it when
    // combined with recurse/force so that `ri` alone (tab completion, etc.)
    // is not a false positive.
    for needle in [
        // Execution policy change
        "set-executionpolicy",
        // User / identity mutation
        "new-localuser",
        "remove-localuser",
        "disable-localuser",
        // Storage / volume destruction
        "clear-recyclebin",
        "format-volume",
        // Elevation via Start-Process
        "start-process powershell -verb runas",
        "start-process pwsh -verb runas",
        "start-process cmd -verb runas",
        "start-process cmd.exe -verb runas",
        // Shutdown / restart (destructive at the session level)
        "stop-computer",
        "restart-computer",
        // Service and scheduled-task deletion
        "remove-service",
        "unregister-scheduledtask",
    ] {
        if lower.contains(needle) {
            return true;
        }
    }
    if has_recursive_force
        && tokens.iter().any(|token| {
            token.eq_ignore_ascii_case("remove-item") || token.eq_ignore_ascii_case("ri")
        })
    {
        return true;
    }

    // cmd.exe destructive commands. Tokenise to avoid matching substrings
    // inside paths.
    let first = tokens.first().copied().unwrap_or("").to_ascii_lowercase();

    match first.as_str() {
        // `del /S` alone (recursive) or `del /Q /F` (quiet + force-delete
        // read-only, without confirmation) are both treated as destructive.
        // Parentheses make the intended precedence explicit.
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

#[cfg(test)]
#[path = "windows_cmd_tests.rs"]
mod tests;
