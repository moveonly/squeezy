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

    // Remove-Item / ri / rm (PowerShell).
    // Destructive when any of:
    //   - valid recurse + force parameters are present (any order)
    //   - -LiteralPath is paired with either recurse or force
    //   - -Confirm:$false suppresses the safety prompt
    //
    // `ri` and `rm` are built-in PowerShell aliases for Remove-Item.
    let is_remove_item = matches!(command_name, "remove-item" | "ri" | "rm");
    if is_remove_item
        && ((powershell_has_recurse_flag(&tokens) && powershell_has_force_flag(&tokens))
            || (powershell_has_literalpath_flag(&tokens)
                && (powershell_has_recurse_flag(&tokens) || powershell_has_force_flag(&tokens)))
            || lower.contains("-confirm:$false")
            || lower.contains("-confirm=false"))
    {
        return true;
    }
    if command_name == "invoke-expression" || contains_iex_alias(&lower) {
        return true;
    }

    // Other unambiguously destructive PowerShell cmdlets.
    if [
        "set-executionpolicy",
        // User/group management
        "new-localuser",
        "remove-localuser",
        "disable-localuser",
        "clear-recyclebin",
        "format-volume",
        "stop-computer",
        "restart-computer",
        "remove-service",
        "unregister-scheduledtask",
        "remove-storedcredential",
        "clear-content",
        "remove-netfirewallrule",
    ]
    .contains(&command_name)
    {
        return true;
    }
    if matches!(command_name, "start-process" | "start") {
        let has_verb_runas = tokens
            .windows(2)
            .any(|pair| pair[0] == "-verb" && pair[1] == "runas")
            || tokens
                .iter()
                .any(|t| matches!(*t, "-verb:runas" | "-verb=runas"));
        if has_verb_runas {
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
        "wmic" => return flag_matches("delete"),
        // Service deletion via sc.exe / net stop+delete
        "sc" | "sc.exe" => return flag_matches("delete"),
        // Scheduled task deletion
        "schtasks" | "schtasks.exe" => return flag_matches("/delete"),
        // TAKEOWN and ICACLS can be used to seize ownership of files and
        // rewrite ACLs wholesale.
        "takeown" => return flag_matches("/r"),
        "icacls" => return flag_matches("/reset") || flag_matches("/grant:r"),
        "attrib" => return flag_matches("/s") && (flag_matches("-r") || flag_matches("+h")),
        "net" if tokens.get(1).copied() == Some("user") => return flag_matches("/delete"),
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

/// True when the token list contains a `-LiteralPath` flag.
fn powershell_has_literalpath_flag(tokens: &[&str]) -> bool {
    tokens
        .iter()
        .skip(1)
        .any(|tok| tok.starts_with("-literalpath"))
}

/// True when the token list contains a `-Recurse` flag or an unambiguous
/// PowerShell abbreviation of it (`-r`, `-re`, `-rec`, …). Explicit-false
/// named-parameter forms (`-Recurse:$false`, `-Recurse:false`, `-Recurse:0`)
/// don't count as Recurse being on.
fn powershell_has_recurse_flag(tokens: &[&str]) -> bool {
    tokens.iter().skip(1).any(|tok| {
        let t = tok.to_ascii_lowercase();
        if powershell_named_flag_is_on(&t, "-recurse") {
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
/// Explicit-false forms (`-Force:$false`) don't count.
fn powershell_has_force_flag(tokens: &[&str]) -> bool {
    tokens.iter().skip(1).any(|tok| {
        let t = tok.to_ascii_lowercase();
        if powershell_named_flag_is_on(&t, "-force") {
            return true;
        }
        matches!(t.as_str(), "-fo" | "-for" | "-forc")
    })
}

/// True when `lower_tok` is the bare flag `name` (e.g. `-force`) or the
/// named-parameter form with a truthy value (`-force:$true`, `-force:true`,
/// `-force:1`). Explicit-false forms (`-force:$false`) and unrelated tokens
/// (`-forced`) return false. `lower_tok` must already be lowercased.
fn powershell_named_flag_is_on(lower_tok: &str, name: &str) -> bool {
    let Some(rest) = lower_tok.strip_prefix(name) else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    let Some(value) = rest.strip_prefix(':') else {
        // `-forced` / `-recursed` etc. are not the named flag.
        return false;
    };
    matches!(value, "$true" | "true" | "1")
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
