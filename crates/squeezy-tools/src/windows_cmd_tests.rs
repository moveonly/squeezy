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
fn flags_remove_item_literalpath_recurse() {
    assert!(is_destructive_windows_segment(
        "Remove-Item -LiteralPath C:\\Users\\foo -Recurse -Force"
    ));
    assert!(is_destructive_windows_segment(
        "Remove-Item -LiteralPath C:\\x -Recurse -Force"
    ));
    assert!(is_destructive_windows_segment(
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
fn flags_remove_item_literalpath() {
    assert!(is_destructive_windows_segment(
        "Remove-Item -LiteralPath C:\\Temp\\file.txt -Force"
    ));
    assert!(is_destructive_windows_segment(
        "remove-item -literalpath 'C:\\Foo' -Recurse"
    ));
}

#[test]
fn flags_ri_alias_recurse_force_orderings() {
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
    assert!(is_destructive_windows_segment("Restart-Computer -Force"));
    assert!(is_destructive_windows_segment(
        "Restart-Computer -Force -Wait"
    ));
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
    assert!(is_destructive_windows_segment(
        "start-process cmd -Verb runAs"
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
}

#[test]
fn flags_unregister_scheduledtask() {
    assert!(is_destructive_windows_segment(
        "Unregister-ScheduledTask -TaskName Foo -Confirm:$false"
    ));
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
    assert!(is_destructive_windows_segment("&iex $cmd"));
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
    assert!(!is_destructive_windows_segment("rm file.log"));
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
    // -LiteralPath without -Force/-Recurse matches the plain Remove-Item
    // policy: a single named file delete is not destructive.
    assert!(!is_destructive_windows_segment(
        "Remove-Item -LiteralPath C:\\Tmp\\file.txt"
    ));
    // Cmdlet names mentioned as ordinary arguments must not trigger the
    // destructive classifier.
    assert!(!is_destructive_windows_segment(
        "Write-Output remove-item -Confirm:$false"
    ));
    assert!(!is_destructive_windows_segment(
        "Write-Output set-executionpolicy"
    ));
    // Remove-Item without both Recurse and Force is not caught by this
    // heuristic unless it uses -LiteralPath or suppresses confirmation.
    // Remove-Item without -Force is not destructive
    assert!(!is_destructive_windows_segment("Remove-Item -Recurse ."));
    // Remove-Item without -Recurse is not caught by this heuristic
    assert!(!is_destructive_windows_segment("Remove-Item -Force ."));
    // ri alias without both flags
    assert!(!is_destructive_windows_segment("ri foo.txt"));
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
fn ri_substring_does_not_false_positive_inside_benign_tokens() {
    // `Invoke-WebRequest -Uri ... -Recurse -Force` lowercases to a stream
    // that contains the bytes `ri -recurse -force` immediately after `-u`,
    // which the previous substring matcher tripped on. The token-based
    // `ri` arm refuses to match unless `ri` is the first whitespace-
    // separated token.
    assert!(!is_destructive_windows_segment(
        "Invoke-WebRequest -Uri https://example.com/api -Recurse -Force"
    ));
    // A bare `ri` without recursion/force flags still classifies safe.
    assert!(!is_destructive_windows_segment("ri foo.txt"));
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
