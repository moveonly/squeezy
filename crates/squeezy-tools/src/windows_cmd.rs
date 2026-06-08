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

    // Remove-Item -LiteralPath is only flagged when paired with -Force or
    // -Recurse, matching the policy applied to plain Remove-Item: a single
    // file delete without -Force is benign.
    if lower.contains("remove-item -literalpath")
        && (lower.contains("-force") || lower.contains("-recurse") || lower.contains(" -r "))
    {
        return true;
    }

    // PowerShell cmdlets where the dangerous shape is unambiguous in the
    // raw text. Each needle is a contiguous substring that does not appear
    // inside benign commands.
    for needle in [
        // Remove-Item recursive/force forms (full name and common aliases ri/rm)
        "remove-item -recurse -force",
        "remove-item -r -force",
        "remove-item -force -recurse",
        "remove-item -force -r",
        // PowerShell aliases for Remove-Item with recurse+force
        "ri -recurse -force",
        "ri -r -force",
        "ri -force -recurse",
        "ri -force -r",
        "rm -recurse -force",
        "rm -r -force",
        "rm -force -recurse",
        "rm -force -r",
        // Privilege / policy escalation
        "set-executionpolicy",
        // User/group management
        "new-localuser",
        // System shutdown / reboot
        "stop-computer",
        "restart-computer",
        // Drive / volume operations
        "clear-recyclebin",
        "format-volume",
        // Arbitrary code execution via expression string (full name)
        "invoke-expression",
        // WMIC destructive operations
        "wmic process delete",
        "wmic product delete",
        // Destructive content operations
        "clear-content",
    ] {
        if lower.contains(needle) {
            return true;
        }
    }

    // PowerShell `iex` alias. Bypass shapes include `iex(...)`,
    // `... | iex`, `;iex`, `&iex`, so checking for the literal `"iex "`
    // substring misses common payloads. Instead, walk every byte and
    // confirm `iex` sits between PowerShell statement/pipeline boundaries
    // (start-of-string, whitespace, `|`, `;`, `&`) and is followed by an
    // argument or pipeline terminator (whitespace, `(`, `'`, `"`, `$`,
    // end-of-string, `|`, `;`, `&`). This catches `iex $cmd`, `| iex`,
    // `iex("...")`, `;iex`, etc. without firing on identifiers like
    // `Get-Hexbin`.
    if contains_iex_alias(&lower) {
        return true;
    }

    // cmd.exe destructive commands. Tokenise to avoid matching substrings
    // inside paths.
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let first = tokens.first().copied().unwrap_or("").to_ascii_lowercase();
    let flag_matches = |flag: &str| tokens.iter().any(|t| t.eq_ignore_ascii_case(flag));

    match first.as_str() {
        // `/s` recurses into subdirectories — the unambiguously destructive
        // case. `/q /f` alone only affects individual files and is too narrow
        // to classify reliably, so we keep the rule precise.
        "del" | "erase" => return flag_matches("/s"),
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

/// True when `lower` contains the PowerShell `iex` alias as a statement
/// or pipeline element. Boundary characters on either side mirror the
/// way PowerShell parses tokens: whitespace, `|`, `;`, `&` separate
/// statements; `(`, `"`, `'`, `$` are valid first characters of the
/// expression argument; end-of-string is also a valid boundary so
/// `cat payload | iex` and a trailing `;iex` both classify.
fn contains_iex_alias(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find("iex") {
        let start = search_from + rel;
        let end = start + 3;
        let before_ok = start == 0 || matches!(bytes[start - 1], b' ' | b'\t' | b'|' | b';' | b'&');
        let after_ok = end == bytes.len()
            || matches!(
                bytes[end],
                b' ' | b'\t' | b'(' | b'\'' | b'"' | b'$' | b'|' | b';' | b'&'
            );
        if before_ok && after_ok {
            return true;
        }
        search_from = start + 1;
    }
    false
}

#[cfg(test)]
#[path = "windows_cmd_tests.rs"]
mod tests;
