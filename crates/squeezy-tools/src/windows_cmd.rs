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

    // PowerShell cmdlets where the dangerous shape is unambiguous in the
    // raw text. Each needle is a contiguous substring that does not appear
    // inside benign commands.
    for needle in [
        // Recursive/forced item removal.
        "remove-item -recurse -force",
        "remove-item -r -force",
        "remove-item -force -recurse",
        "remove-item -force -r",
        // Abbreviation "ri" is a built-in alias for Remove-Item.
        "ri -recurse -force",
        "ri -force -recurse",
        "ri -r -force",
        "ri -force -r",
        // Policy / user / storage management.
        "set-executionpolicy",
        "new-localuser",
        "remove-localuser",
        "clear-recyclebin",
        "format-volume",
        // Credential and secret stores.
        "remove-storedcredential",
        "clear-content",
        // Network / firewall rule removal.
        "remove-netfirewallrule",
        // Service and scheduled-task removal.
        "remove-service",
        "unregister-scheduledtask",
    ] {
        if lower.contains(needle) {
            return true;
        }
    }

    // cmd.exe destructive commands. Tokenise to avoid matching substrings
    // inside paths.
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let first = tokens.first().copied().unwrap_or("").to_ascii_lowercase();
    let flag_matches = |flag: &str| tokens.iter().any(|t| t.eq_ignore_ascii_case(flag));

    match first.as_str() {
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
        // TAKEOWN and ICACLS can be used to seize ownership of files and
        // rewrite ACLs wholesale — destructive when applied recursively.
        "takeown" => return flag_matches("/r"),
        "icacls" => return flag_matches("/reset") || flag_matches("/grant:r"),
        // ATTRIB with /S recursively changes file attributes (e.g. stripping
        // read-only flags before a subsequent delete).
        "attrib" => return flag_matches("/s") && (flag_matches("-r") || flag_matches("+h")),
        // NET USER with /DELETE removes a local account.
        "net" => {
            if tokens
                .get(1)
                .map(|t| t.eq_ignore_ascii_case("user"))
                .unwrap_or(false)
            {
                return flag_matches("/delete");
            }
        }
        _ => {}
    }

    false
}

#[cfg(test)]
#[path = "windows_cmd_tests.rs"]
mod tests;
