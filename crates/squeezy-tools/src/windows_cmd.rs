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

    // Fast-path substring checks for unambiguous contiguous PowerShell shapes
    // that cannot appear inside benign commands.
    for needle in [
        "set-executionpolicy",
        "new-localuser",
        "clear-recyclebin",
        "format-volume",
        // Invoke-Expression / its alias iex can execute arbitrary code.
        "invoke-expression",
    ] {
        if lower.contains(needle) {
            return true;
        }
    }

    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let first = tokens.first().copied().unwrap_or("").to_ascii_lowercase();

    // ── PowerShell Remove-Item family ────────────────────────────────────────
    // Matches: Remove-Item, ri (alias). Requires BOTH a -Recurse (or
    // abbreviation / alias) AND a -Force (or abbreviation) flag to be present
    // anywhere in the token list, in any order.
    if matches!(first.as_str(), "remove-item" | "ri") {
        if powershell_has_recurse_flag(&tokens) && powershell_has_force_flag(&tokens) {
            return true;
        }
    }

    // `iex` is the PowerShell built-in alias for Invoke-Expression.
    if first == "iex" {
        return true;
    }

    // Start-Process with -Verb RunAs requests elevation; treat as high-risk
    // destructive so the user sees the critical approval path.
    if first == "start-process" || first == "start" {
        let has_verb_runas = tokens.windows(2).any(|pair| {
            pair[0].eq_ignore_ascii_case("-verb") && pair[1].eq_ignore_ascii_case("runas")
        }) || tokens.iter().any(|t| {
            t.eq_ignore_ascii_case("-verb:runas") || t.eq_ignore_ascii_case("-verb=runas")
        });
        if has_verb_runas {
            return true;
        }
    }

    // ── cmd.exe destructive commands ─────────────────────────────────────────
    // Tokenise to avoid matching substrings inside paths.
    let flag_matches = |flag: &str| tokens.iter().any(|t| t.eq_ignore_ascii_case(flag));

    match first.as_str() {
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
/// PowerShell abbreviation of it.
fn powershell_has_force_flag(tokens: &[&str]) -> bool {
    tokens.iter().skip(1).any(|tok| {
        let t = tok.to_ascii_lowercase();
        if t.starts_with("-force") {
            return true;
        }
        matches!(t.as_str(), "-f" | "-fo" | "-for" | "-forc")
    })
}

#[cfg(test)]
#[path = "windows_cmd_tests.rs"]
mod tests;
