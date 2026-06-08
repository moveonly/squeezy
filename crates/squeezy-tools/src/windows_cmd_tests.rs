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
fn flags_remove_item_literalpath_recurse() {
    // Parameter-interleaved form: -LiteralPath before -Recurse -Force.
    assert!(is_destructive_windows_segment(
        "Remove-Item -LiteralPath C:\\x -Recurse -Force"
    ));
    assert!(is_destructive_windows_segment(
        "remove-item -literalpath C:\\x -r"
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
fn flags_ri_alias() {
    // `ri` is the built-in PowerShell alias for Remove-Item.
    assert!(is_destructive_windows_segment("ri -r C:\\tmp"));
    assert!(is_destructive_windows_segment("ri -Recurse C:\\data"));
    assert!(is_destructive_windows_segment(
        "ri C:\\file -Confirm:$false"
    ));
}

#[test]
fn flags_remove_item_short_recurse_alias() {
    // -r is the short alias for -Recurse in PowerShell.
    assert!(is_destructive_windows_segment("Remove-Item -r C:\\foo"));
    assert!(is_destructive_windows_segment(
        "remove-item -force -r C:\\bar"
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
    // `ri` without recursive/confirm-suppress flags is not flagged.
    assert!(!is_destructive_windows_segment("ri C:\\foo\\bar.txt"));
    // Remove-Item without -Recurse/-r or -Confirm:$false is not flagged.
    assert!(!is_destructive_windows_segment(
        "Remove-Item C:\\logs\\app.log"
    ));
}
