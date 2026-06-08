use super::ShellProgram;

#[test]
fn pwsh_bare_name_gets_powershell_args() {
    let args = ShellProgram::args_for_override("pwsh", "echo hi");
    assert_eq!(args[0], "-NoLogo");
    assert_eq!(args[1], "-NoProfile");
    assert_eq!(args[2], "-Command");
    assert_eq!(args[3], "echo hi");
}

#[test]
fn powershell_bare_name_gets_powershell_args() {
    let args = ShellProgram::args_for_override("powershell", "echo hi");
    assert_eq!(args[0], "-NoLogo");
    assert_eq!(args[1], "-NoProfile");
    assert_eq!(args[2], "-Command");
}

#[test]
fn cmd_bare_name_gets_cmd_args() {
    let args = ShellProgram::args_for_override("cmd", "dir");
    assert_eq!(args[0], "/D");
    assert_eq!(args[1], "/S");
    assert_eq!(args[2], "/C");
}

#[test]
fn pwsh_exe_path_gets_powershell_args() {
    let args = ShellProgram::args_for_override(
        r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
        "echo hi",
    );
    assert_eq!(args[0], "-NoLogo");
    assert_eq!(args[2], "-Command");
}

#[test]
fn unknown_shell_gets_lc_args() {
    let args = ShellProgram::args_for_override("/usr/local/bin/fish", "echo hi");
    assert_eq!(args[0], "-lc");
    assert_eq!(args[1], "echo hi");
}
