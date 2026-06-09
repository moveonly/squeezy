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
    // `-F` is not a valid abbreviation (ambiguous with -Filter), so the
    // command is not classified as destructive unless a valid force form is used.
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
fn flags_remove_item_literalpath_recurse() {
    assert!(is_destructive_windows_segment(
        "Remove-Item -LiteralPath C:\\Users\\foo -Recurse -Force"
    ));
    assert!(is_destructive_windows_segment(
        "Remove-Item -LiteralPath C:\\x -Recurse -Force"
    ));
    assert!(!is_destructive_windows_segment(
        "remove-item -literalpath C:\\x -r"
    ));
}

#[test]
fn flags_remove_item_path_flag() {
    assert!(is_destructive_windows_segment(
        "Remove-Item -Path C:\\tmp\\logs -Force -Recurse"
    ));
}

#[test]
fn flags_remove_item_confirm_false() {
    // -Confirm:$false suppresses safety prompt; treated as destructive.
    assert!(is_destructive_windows_segment(
        "Remove-Item -Recurse -Confirm:$false C:\\logs"
    ));
    assert!(is_destructive_windows_segment(
        "Remove-Item C:\\file -Confirm:$false"
    ));
}

#[test]
fn flags_ri_alias_recurse_force() {
    assert!(is_destructive_windows_segment(
        "ri -Recurse -Force C:\\data"
    ));
    assert!(is_destructive_windows_segment(
        "ri -Force -Recurse C:\\data"
    ));
    assert!(is_destructive_windows_segment("ri -r -Force C:\\data"));
    // `ri` is the built-in PowerShell alias for Remove-Item.
    assert!(!is_destructive_windows_segment("ri -r C:\\tmp"));
    assert!(!is_destructive_windows_segment("ri -Recurse C:\\data"));
    assert!(is_destructive_windows_segment(
        "ri C:\\file -Confirm:$false"
    ));
}

#[test]
fn flags_remove_item_short_recurse_alias() {
    // -r is the short alias for -Recurse in PowerShell.
    assert!(!is_destructive_windows_segment("Remove-Item -r C:\\foo"));
    assert!(is_destructive_windows_segment(
        "remove-item -force -r C:\\bar"
    ));
}

#[test]
fn flags_invoked_and_module_qualified_remove_item() {
    assert!(!is_destructive_windows_segment(
        "& Remove-Item -Recurse C:\\tmp"
    ));
    assert!(!is_destructive_windows_segment(
        "Microsoft.PowerShell.Management\\Remove-Item -r C:\\tmp"
    ));
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
    assert!(is_destructive_windows_segment("Restart-Computer -Force"));
}

#[test]
fn flags_remove_localuser() {
    assert!(is_destructive_windows_segment(
        "Remove-LocalUser -Name testuser"
    ));
}

#[test]
fn flags_service_deletion() {
    assert!(is_destructive_windows_segment("sc delete MySvc"));
    assert!(is_destructive_windows_segment("sc.exe delete MySvc"));
}

#[test]
fn flags_schtasks_delete() {
    assert!(is_destructive_windows_segment(
        "schtasks /delete /tn MyTask /f"
    ));
}

#[test]
fn flags_shutdown_commands() {
    assert!(is_destructive_windows_segment("shutdown /s /t 0"));
    assert!(is_destructive_windows_segment("shutdown /r /t 0"));
}

#[test]
fn flags_new_and_disable_localuser() {
    assert!(is_destructive_windows_segment("New-LocalUser -Name foo"));
    assert!(is_destructive_windows_segment(
        "Disable-LocalUser -Name foo"
    ));
}

#[test]
fn flags_clear_recyclebin() {
    assert!(is_destructive_windows_segment(
        "Clear-RecycleBin -Force -Confirm:$false"
    ));
}

#[test]
fn flags_format_volume() {
    assert!(is_destructive_windows_segment(
        "Format-Volume -DriveLetter C -Force"
    ));
}

#[test]
fn flags_remove_service() {
    assert!(is_destructive_windows_segment("Remove-Service -Name MySvc"));
}

#[test]
fn flags_bcdedit_deletevalue() {
    assert!(is_destructive_windows_segment(
        "bcdedit /deletevalue {default} safeboot"
    ));
}

#[test]
fn flags_start_process_runas() {
    assert!(is_destructive_windows_segment(
        "Start-Process powershell -Verb RunAs"
    ));
    assert!(is_destructive_windows_segment(
        "Start-Process pwsh -Verb RunAs"
    ));
    assert!(is_destructive_windows_segment(
        "Start-Process cmd -Verb RunAs"
    ));
    assert!(is_destructive_windows_segment(
        "Start-Process cmd.exe -Verb RunAs"
    ));
}

#[test]
fn flags_del_quiet_force_without_recursive() {
    // /Q /F together is also flagged as destructive (quiet + force-delete
    // read-only files) even without /S — documents the deliberate precedence.
    assert!(is_destructive_windows_segment("del /Q /F C:\\tmp"));
}

#[test]
fn ignores_del_without_s_q_f() {
    // A plain del that is neither recursive nor force+quiet must not trigger.
    assert!(!is_destructive_windows_segment("del /Q foo.txt"));
    assert!(!is_destructive_windows_segment("del /F foo.txt"));
}

#[test]
fn ignores_remove_item_path_without_recursive_force() {
    assert!(!is_destructive_windows_segment(
        "Remove-Item -Path C:\\tmp\\foo.txt"
    ));
    assert!(!is_destructive_windows_segment(
        "Remove-Item -LiteralPath C:\\tmp\\foo.txt -Force"
    ));
}

#[test]
fn flags_unregister_scheduledtask() {
    assert!(is_destructive_windows_segment(
        "Unregister-ScheduledTask -TaskName Foo -Confirm:$false"
    ));
}

#[test]
fn flags_recursive_del() {
    assert!(is_destructive_windows_segment("del /S C:\\tmp"));
    assert!(is_destructive_windows_segment("del /Q /F /S C:\\tmp"));
}

#[test]
fn flags_del_quiet_force_without_recurse() {
    // /Q /F together (no /S): suppresses prompt and forces deletion of
    // read-only files. Intentionally flagged as destructive even without
    // /S because the operation is non-interactive and hard to recover.
    assert!(is_destructive_windows_segment(
        "del /Q /F C:\\important.txt"
    ));
    // /Q alone (no /F) is not flagged — deleting with confirmation suppressed
    // but without forcing read-only removal is borderline; keep narrow.
    assert!(!is_destructive_windows_segment("del /Q C:\\file.txt"));
    // /F alone is similarly not flagged.
    assert!(!is_destructive_windows_segment("del /F C:\\file.txt"));
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
    // `ri` alone (e.g. during tab completion) must not trigger.
    assert!(!is_destructive_windows_segment("ri C:\\tmp\\foo.txt"));
    // `sc` with a benign subcommand must not trigger.
    assert!(!is_destructive_windows_segment("sc query MySvc"));
    assert!(!is_destructive_windows_segment("sc start MySvc"));
    // `schtasks` without /delete must not trigger.
    assert!(!is_destructive_windows_segment("schtasks /query /fo LIST"));
    // shutdown with only /t must not trigger.
    assert!(!is_destructive_windows_segment("shutdown /t 60"));
    // `ri` without recursive/confirm-suppress flags is not flagged.
    assert!(!is_destructive_windows_segment("ri C:\\foo\\bar.txt"));
    // Remove-Item without -Recurse/-r or -Confirm:$false is not flagged.
    assert!(!is_destructive_windows_segment(
        "Remove-Item C:\\logs\\app.log"
    ));
    // Cmdlet names mentioned as ordinary arguments must not trigger the
    // destructive classifier.
    assert!(!is_destructive_windows_segment(
        "Write-Output remove-item -Confirm:$false"
    ));
    assert!(!is_destructive_windows_segment(
        "Write-Output set-executionpolicy"
    ));
    // Remove-Item without -Recurse is not caught by this heuristic
    assert!(!is_destructive_windows_segment("Remove-Item -Force ."));
    // ri alias without both flags
    assert!(!is_destructive_windows_segment("ri foo.txt"));
}
