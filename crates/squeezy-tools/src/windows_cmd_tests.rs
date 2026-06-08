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
        "Remove-Item -LiteralPath C:\\Users\\foo -Recurse -Force"
    ));
}

#[test]
fn flags_remove_item_path_flag() {
    assert!(is_destructive_windows_segment(
        "Remove-Item -Path C:\\tmp\\logs -Force -Recurse"
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
fn flags_start_process_runas() {
    assert!(is_destructive_windows_segment(
        "Start-Process powershell -Verb RunAs"
    ));
    assert!(is_destructive_windows_segment(
        "Start-Process pwsh -Verb RunAs"
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
}
