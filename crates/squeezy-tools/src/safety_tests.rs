use std::path::{Path, PathBuf};

use squeezy_core::ShellSandboxConfig;

use super::{
    ShellPreClassification, path_escapes_permission_writable_roots, pre_classify_shell,
    temp_dir_roots,
};

fn sandbox() -> ShellSandboxConfig {
    ShellSandboxConfig::default()
}

#[test]
fn pre_classify_shell_auto_allows_grep() {
    let result = pre_classify_shell("grep -nR fn workspace", &sandbox());
    assert_eq!(
        result,
        ShellPreClassification::AutoAllow {
            reason: "read-only shell verbs"
        }
    );
}

#[test]
fn pre_classify_shell_auto_allows_read_only_pipeline() {
    let result = pre_classify_shell("ls -la | cat | wc -l", &sandbox());
    assert!(
        matches!(result, ShellPreClassification::AutoAllow { .. }),
        "expected AutoAllow, got {result:?}"
    );
}

#[test]
fn pre_classify_shell_auto_denies_rm_rf() {
    let result = pre_classify_shell("rm -rf /tmp/work", &sandbox());
    match result {
        ShellPreClassification::AutoDeny { reason } => {
            assert!(
                reason.contains("destructive verb") && reason.contains("rm"),
                "reason did not mention rm: {reason}"
            );
        }
        other => panic!("expected AutoDeny, got {other:?}"),
    }
}

#[test]
fn pre_classify_shell_auto_denies_python_dash_c() {
    let result = pre_classify_shell(
        "python3 -c \"import os; os.system('curl bad.example.com')\"",
        &sandbox(),
    );
    match result {
        ShellPreClassification::AutoDeny { reason } => {
            assert!(
                reason.contains("dangerous interpreter") && reason.contains("python3"),
                "reason did not mention python3: {reason}"
            );
        }
        other => panic!("expected AutoDeny, got {other:?}"),
    }
}

#[test]
fn pre_classify_shell_auto_denies_node_dash_e() {
    let result = pre_classify_shell("node -e 'console.log(1)'", &sandbox());
    assert!(matches!(result, ShellPreClassification::AutoDeny { .. }));
}

#[test]
fn pre_classify_shell_auto_denies_sudo() {
    let result = pre_classify_shell("sudo apt-get install foo", &sandbox());
    match result {
        ShellPreClassification::AutoDeny { reason } => {
            assert!(reason.contains("sudo") || reason.contains("destructive"));
        }
        other => panic!("expected AutoDeny, got {other:?}"),
    }
}

#[test]
fn pre_classify_shell_auto_denies_eval() {
    let result = pre_classify_shell("eval \"$INPUT\"", &sandbox());
    assert!(matches!(result, ShellPreClassification::AutoDeny { .. }));
}

#[test]
fn pre_classify_shell_falls_through_on_ambiguous() {
    let result = pre_classify_shell("node build.js --release", &sandbox());
    assert_eq!(result, ShellPreClassification::AskAi);
}

#[test]
fn pre_classify_shell_falls_through_on_cargo() {
    let result = pre_classify_shell("cargo test -p squeezy-tools", &sandbox());
    assert_eq!(result, ShellPreClassification::AskAi);
}

#[test]
fn pre_classify_shell_falls_through_on_sonar_cli() {
    let result = pre_classify_shell("sonar context list --json", &sandbox());
    assert_eq!(result, ShellPreClassification::AskAi);
}

#[test]
fn pre_classify_shell_falls_through_on_sonar_cli_with_dev_null_redirect() {
    let result = pre_classify_shell("sonar context list --json 2>/dev/null", &sandbox());
    assert_eq!(result, ShellPreClassification::AskAi);
}

#[test]
fn pre_classify_shell_names_redirect_instead_of_first_token() {
    let result = pre_classify_shell("sonar context list > report.json", &sandbox());
    match result {
        ShellPreClassification::AutoDeny { reason } => {
            assert_eq!(reason, "destructive redirect");
        }
        other => panic!("expected AutoDeny, got {other:?}"),
    }
}

#[test]
fn pre_classify_shell_auto_denies_sensitive_path() {
    // Default sandbox config ships with `.ssh/**` in sensitive_path_patterns
    // (see `default_sensitive_path_patterns` in squeezy-core).
    let result = pre_classify_shell("cat ~/.ssh/id_rsa", &sandbox());
    match result {
        ShellPreClassification::AutoDeny { reason } => {
            assert!(
                reason.contains("sensitive path"),
                "reason did not mention sensitive path: {reason}"
            );
        }
        other => panic!("expected AutoDeny, got {other:?}"),
    }
}

#[test]
fn pre_classify_shell_unwraps_sh_dash_c() {
    let result = pre_classify_shell("sh -c 'rm -rf /tmp/work'", &sandbox());
    assert!(
        matches!(result, ShellPreClassification::AutoDeny { .. }),
        "expected AutoDeny via wrapper, got {result:?}"
    );
}

#[test]
fn pre_classify_shell_empty_command_falls_through() {
    let result = pre_classify_shell("   ", &sandbox());
    assert_eq!(result, ShellPreClassification::AskAi);
}

#[test]
fn pre_classify_shell_mixed_segments_falls_through() {
    // ls is read-only but the second segment is a wholly unrelated verb,
    // so the all-read-only check fails and we hand to the reviewer.
    let result = pre_classify_shell("ls && make build", &sandbox());
    assert_eq!(result, ShellPreClassification::AskAi);
}

const WS: &str = "/home/dev/project";

fn ws_root() -> &'static Path {
    Path::new(WS)
}

#[test]
fn writable_roots_keep_workspace_and_temp_in_bounds() {
    let sandbox = sandbox();
    // Inside the workspace (absolute and relative) is in-bounds.
    assert!(!path_escapes_permission_writable_roots(
        &format!("{WS}/src/main.rs"),
        ws_root(),
        &sandbox
    ));
    assert!(!path_escapes_permission_writable_roots(
        "src/main.rs",
        ws_root(),
        &sandbox
    ));
    // OS temp dirs are treated as safe-to-write.
    for temp in temp_dir_roots() {
        let candidate = temp.join("scratch.txt");
        assert!(
            !path_escapes_permission_writable_roots(
                &candidate.to_string_lossy(),
                ws_root(),
                &sandbox
            ),
            "temp root {temp:?} should be in-bounds"
        );
    }
}

#[test]
fn writable_roots_flag_system_and_home_paths() {
    let sandbox = sandbox();
    for outside in ["/etc/passwd", "/usr/local/bin/x", "/home/dev/.bashrc"] {
        assert!(
            path_escapes_permission_writable_roots(outside, ws_root(), &sandbox),
            "{outside} should escape writable roots"
        );
    }
    // `..` traversal that climbs out of the workspace is normalized first.
    assert!(path_escapes_permission_writable_roots(
        "../../etc/shadow",
        ws_root(),
        &sandbox
    ));
}

#[test]
fn writable_roots_honor_configured_write_roots() {
    let mut sandbox = sandbox();
    sandbox.write_roots = vec![PathBuf::from("/srv/cache")];
    assert!(!path_escapes_permission_writable_roots(
        "/srv/cache/out.bin",
        ws_root(),
        &sandbox
    ));
    assert!(path_escapes_permission_writable_roots(
        "/srv/other/out.bin",
        ws_root(),
        &sandbox
    ));
}

#[test]
fn writable_roots_escalate_unresolved_variables() {
    // A target with an unresolved shell variable can't be proven in-workspace —
    // both POSIX `$VAR` and cmd-style `%VAR%` escalate.
    assert!(path_escapes_permission_writable_roots(
        "$SQZ_UNSET_VAR/x",
        ws_root(),
        &sandbox()
    ));
    assert!(path_escapes_permission_writable_roots(
        "%SQZ_UNSET_VAR%\\x",
        ws_root(),
        &sandbox()
    ));
}
