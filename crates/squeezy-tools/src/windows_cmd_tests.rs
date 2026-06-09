use super::is_destructive_windows_segment;

#[test]
fn flags_powershell_recursive_force_remove() {
    assert!(is_destructive_windows_segment(
        "Remove-Item -Recurse -Force C:\\Users\\foo"
    ));
    assert!(is_destructive_windows_segment(
        "remove-item -force -recurse C:\\data"
    ));
    // -Path / -LiteralPath with reordered flags (Bug 2)
    assert!(is_destructive_windows_segment(
        "Remove-Item -Path . -Force -Recurse"
    ));
    assert!(is_destructive_windows_segment(
        "Remove-Item -LiteralPath . -Recurse -Force"
    ));
}

#[test]
fn flags_powershell_ri_alias() {
    // Bug 2: `ri` alias for Remove-Item
    assert!(is_destructive_windows_segment("ri . -Recurse -Force"));
    assert!(is_destructive_windows_segment("RI -Force -Recurse C:\\tmp"));
}

#[test]
fn flags_powershell_abbreviated_params() {
    // Bug 2: abbreviated -Re / -R / -Fo parameter forms
    assert!(is_destructive_windows_segment("Remove-Item . -Re -Force"));
    assert!(is_destructive_windows_segment("Remove-Item . -R -Force"));
    assert!(is_destructive_windows_segment("Remove-Item . -Recurse -Fo"));
    assert!(is_destructive_windows_segment("Remove-Item . -Re -Fo"));
    // `-F` is not a valid abbreviation (ambiguous with -Filter); ri . -R -F
    // would fail at the PowerShell runtime but we do not classify it as
    // destructive since the conservative policy is: only flag *valid* forms.
    assert!(!is_destructive_windows_segment("ri . -R -F"));
}

#[test]
fn flags_invoke_expression() {
    assert!(is_destructive_windows_segment(
        "Invoke-Expression 'rm -rf /'"
    ));
    assert!(is_destructive_windows_segment("invoke-expression $cmd"));
    assert!(is_destructive_windows_segment("iex 'rm -rf /'"));
    assert!(is_destructive_windows_segment("IEX 'malicious'"));
}

#[test]
fn flags_start_process_runas() {
    assert!(is_destructive_windows_segment(
        "Start-Process pwsh -Verb RunAs"
    ));
    assert!(is_destructive_windows_segment(
        "start-process cmd -Verb runAs"
    ));
}

#[test]
fn flags_set_executionpolicy() {
    assert!(is_destructive_windows_segment(
        "Set-ExecutionPolicy -ExecutionPolicy Bypass -Scope Process"
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
    assert!(!is_destructive_windows_segment("dir /S"));
    assert!(!is_destructive_windows_segment("Get-ChildItem -Recurse"));
    assert!(!is_destructive_windows_segment("echo hello"));
    assert!(!is_destructive_windows_segment("cargo build"));
    // Remove-Item without -Force is not destructive
    assert!(!is_destructive_windows_segment("Remove-Item -Recurse ."));
    // Remove-Item without -Recurse is not caught by this heuristic
    assert!(!is_destructive_windows_segment("Remove-Item -Force ."));
    // ri alias without both flags
    assert!(!is_destructive_windows_segment("ri foo.txt"));
}

#[test]
fn explicit_false_named_flags_do_not_count_as_on() {
    // `-Force:$false` / `-Recurse:$false` are real PowerShell idioms for
    // explicitly turning the switch off. They must not classify as forced
    // recursive deletion.
    assert!(!is_destructive_windows_segment(
        "Remove-Item -Recurse:$false -Force ."
    ));
    assert!(!is_destructive_windows_segment(
        "Remove-Item -Recurse -Force:$false ."
    ));
    assert!(!is_destructive_windows_segment(
        "Remove-Item -Recurse:$false -Force:$false ."
    ));
    // `-Force:$true` / `-Force:true` / `-Force:1` still count as on.
    assert!(is_destructive_windows_segment(
        "Remove-Item -Recurse:$true -Force:$true ."
    ));
    assert!(is_destructive_windows_segment(
        "Remove-Item -Recurse:true -Force:true ."
    ));
    assert!(is_destructive_windows_segment(
        "Remove-Item -Recurse:1 -Force:1 ."
    ));
}
