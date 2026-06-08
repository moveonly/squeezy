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
}

#[test]
fn flags_powershell_remove_item_short_alias() {
    assert!(is_destructive_windows_segment("ri -Recurse -Force C:\\tmp"));
    assert!(is_destructive_windows_segment(
        "ri -force -recurse C:\\data"
    ));
}

#[test]
fn flags_remove_local_user() {
    assert!(is_destructive_windows_segment("Remove-LocalUser -Name foo"));
}

#[test]
fn flags_unregister_scheduled_task() {
    assert!(is_destructive_windows_segment(
        "Unregister-ScheduledTask -TaskName backup -Confirm:$false"
    ));
}

#[test]
fn flags_takeown_recursive() {
    assert!(is_destructive_windows_segment("takeown /f C:\\dir /r"));
}

#[test]
fn flags_net_user_delete() {
    assert!(is_destructive_windows_segment("net user bob /delete"));
}

#[test]
fn does_not_flag_safe_takeown() {
    // /r is required for our match; single-file takeown is less dangerous
    assert!(!is_destructive_windows_segment("takeown /f somefile.txt"));
}

#[test]
fn does_not_flag_safe_net_user_add() {
    assert!(!is_destructive_windows_segment(
        "net user bob Password1 /add"
    ));
}
