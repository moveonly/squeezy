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
    // Pre-tokenise once for both PowerShell and cmd.exe checks.
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    let first = tokens.first().copied().unwrap_or("");

    // ── Remove-Item / ri (PowerShell) ────────────────────────────────────
    // Destructive when any of:
    //   • -Recurse or its short alias -r is present (any parameter position)
    //   • -Confirm:$false suppresses the safety prompt
    //
    // The `ri` check matches the built-in PowerShell alias; `rm` / `rmdir`
    // are already flagged by the POSIX destructive-verb list in
    // `destructive_shell_segment_reason`.
    let is_remove_item = lower.contains("remove-item") || first == "ri";
    if is_remove_item {
        let has_recurse = tokens.iter().any(|t| *t == "-recurse" || *t == "-r");
        if has_recurse || lower.contains("-confirm:$false") {
            return true;
        }
    }

    // ── Other unambiguously destructive PowerShell cmdlets ───────────────
    for needle in [
        "set-executionpolicy",
        "new-localuser",
        "clear-recyclebin",
        "format-volume",
    ] {
        if lower.contains(needle) {
            return true;
        }
    }

    // ── cmd.exe destructive commands ─────────────────────────────────────
    // Tokenise to avoid matching substrings inside paths.
    let flag_matches = |flag: &str| tokens.contains(&flag);

    match first {
        "del" | "erase" => return flag_matches("/s") || flag_matches("/q") && flag_matches("/f"),
        "rd" | "rmdir" => return flag_matches("/s"),
        "format" | "diskpart" => return true,
        "vssadmin" => return flag_matches("delete"),
        "bcdedit" => {
            return flag_matches("/delete") || flag_matches("/deletevalue");
        }
        "reg" => return flag_matches("delete"),
        "cipher" => return flag_matches("/w"),
        _ => {}
    }

    false
}

#[cfg(test)]
#[path = "windows_cmd_tests.rs"]
mod tests;
