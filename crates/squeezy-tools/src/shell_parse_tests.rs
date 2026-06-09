//! Unit tests for the typed `CommandUnit` payload produced by
//! `extract_command_units`. A structured walk should surface
//! `{ name, args, env, redirects, has_substitution }` per `command` node
//! so downstream classifiers can stop re-splitting segment text via
//! `split_whitespace`.

use super::{
    CommandUnit, PlanModeShellSafety, Redirect, analyze_shell_command, extract_command_units,
    extract_shell_write_targets, shell_segment_has_destructive_redirect,
};
use crate::PermissionCapability;

#[test]
fn extract_commands_returns_units() {
    let units = extract_command_units("FOO=bar rm -rf \"/tmp/x y\" 2>/dev/null");
    assert_eq!(units.len(), 1, "expected single command unit");
    let unit = &units[0];
    assert_eq!(unit.name, "rm");
    assert_eq!(unit.args, vec!["-rf".to_string(), "/tmp/x y".to_string()]);
    assert_eq!(unit.env, vec![("FOO".to_string(), "bar".to_string())]);
    assert_eq!(
        unit.redirects,
        vec![Redirect {
            op: ">".to_string(),
            target: "/dev/null".to_string(),
            fd: Some(2),
        }]
    );
    assert!(!unit.has_substitution);
}

#[test]
fn extract_commands_splits_pipeline_into_units() {
    let units = extract_command_units("rg needle | xargs rm -rf target");
    assert_eq!(units.len(), 2);
    assert_eq!(units[0].name, "rg");
    assert_eq!(units[0].args, vec!["needle".to_string()]);
    assert_eq!(units[1].name, "xargs");
    assert_eq!(
        units[1].args,
        vec!["rm".to_string(), "-rf".to_string(), "target".to_string()]
    );
}

#[test]
fn extract_commands_captures_multiple_env_assignments() {
    let units = extract_command_units("FOO=1 BAR=two cargo test --workspace");
    assert_eq!(units.len(), 1);
    let unit = &units[0];
    assert_eq!(unit.name, "cargo");
    assert_eq!(
        unit.args,
        vec!["test".to_string(), "--workspace".to_string()]
    );
    assert_eq!(
        unit.env,
        vec![
            ("FOO".to_string(), "1".to_string()),
            ("BAR".to_string(), "two".to_string()),
        ]
    );
}

#[test]
fn extract_commands_captures_append_and_stdout_redirects() {
    let units = extract_command_units("echo hi >> out.log 1>err.log");
    assert_eq!(units.len(), 1);
    let unit = &units[0];
    assert_eq!(unit.name, "echo");
    assert_eq!(unit.args, vec!["hi".to_string()]);
    assert!(
        unit.redirects
            .iter()
            .any(|r| r.op == ">>" && r.target == "out.log" && r.fd.is_none()),
        "missing append redirect, got {:?}",
        unit.redirects
    );
    assert!(
        unit.redirects
            .iter()
            .any(|r| r.op == ">" && r.target == "err.log" && r.fd == Some(1)),
        "missing fd 1 redirect, got {:?}",
        unit.redirects
    );
}

#[test]
fn extract_commands_marks_substitution_units() {
    let units = extract_command_units("echo $(date)");
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].name, "echo");
    assert!(
        units[0].has_substitution,
        "command substitution should be flagged"
    );
}

#[test]
fn extract_commands_keeps_quoted_args_intact() {
    let units = extract_command_units("grep 'foo bar' src/lib.rs");
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].name, "grep");
    assert_eq!(
        units[0].args,
        vec!["foo bar".to_string(), "src/lib.rs".to_string()],
        "outer quotes must be stripped without splitting the token"
    );
}

#[test]
fn extract_commands_returns_empty_on_unparseable_input() {
    let units = extract_command_units("");
    assert!(units.is_empty());
}

#[test]
fn extract_commands_handles_compound_andand() {
    let units = extract_command_units("cargo fmt && cargo test");
    assert_eq!(units.len(), 2);
    assert_eq!(units[0].name, "cargo");
    assert_eq!(units[0].args, vec!["fmt".to_string()]);
    assert_eq!(units[1].name, "cargo");
    assert_eq!(units[1].args, vec!["test".to_string()]);
}

#[test]
fn js_arrow_inside_heredoc_body_is_not_destructive_redirect() {
    // When tree-sitter cannot resolve the heredoc to a single command node,
    // analysis falls back to byte-scanning the raw text. A JS arrow `=>`
    // inside a `node - <<'NODE' ... NODE` heredoc body must NOT be
    // classified as a `>` file-redirect. Trace evidence: 3/3 no-graph JS
    // realworld eval runs denied this shape via the pre-classifier.
    let cmd = "node - <<'NODE'\nconst x = arr.filter(f => f.endsWith('.js'));\nNODE";
    assert!(
        !shell_segment_has_destructive_redirect(cmd),
        "arrow `=>` must not register as a file redirect",
    );
    let analysis = analyze_shell_command(cmd);
    assert!(
        !analysis.destructive,
        "node heredoc with a JS arrow body must not be flagged destructive: {analysis:?}",
    );
    assert_ne!(analysis.capability, PermissionCapability::Destructive);
}

#[test]
fn sonar_cli_is_not_destructive() {
    let analysis = analyze_shell_command("sonar context list --json");
    assert!(
        !analysis.destructive,
        "sonar CLI reads must not be flagged destructive: {analysis:?}",
    );
    assert_ne!(analysis.capability, PermissionCapability::Destructive);
}

#[test]
fn plan_mode_treats_sed_print_slices_as_read_only() {
    for command in [
        "sed -n '1,5p' file.txt",
        "sed -n 1,5p file.txt",
        "cat -n file.txt | sed -n '50,200p'",
    ] {
        assert_eq!(
            super::classify_plan_mode_shell_command(command),
            PlanModeShellSafety::ReadOnly,
            "{command} should be a read-only Plan Mode probe"
        );
    }
}

#[test]
fn plan_mode_rejects_mutating_sed_in_place() {
    assert_eq!(
        super::classify_plan_mode_shell_command("sed -i 's/a/b/' file.txt"),
        PlanModeShellSafety::Mutating
    );
}

#[test]
fn plan_mode_does_not_treat_sed_extra_scripts_as_read_only() {
    assert_eq!(
        super::classify_plan_mode_shell_command("sed -n '1p' -e 's/a/b/w out.txt' input.txt"),
        PlanModeShellSafety::NeedsApproval
    );
}

#[test]
fn plan_mode_treats_codex_style_read_only_family_as_read_only() {
    for command in [
        "nl file.txt",
        "paste a.txt b.txt",
        "rev file.txt",
        "seq 1 10",
        "uname -a",
        "which cargo",
        "whoami",
        "base64 file.bin",
    ] {
        assert_eq!(
            super::classify_plan_mode_shell_command(command),
            PlanModeShellSafety::ReadOnly,
            "{command} should be a read-only Plan Mode probe"
        );
    }
}

#[test]
fn plan_mode_rejects_base64_file_output() {
    for command in [
        "base64 file.bin --output out.txt",
        "base64 file.bin --output=out.txt",
        "base64 file.bin -oout.txt",
    ] {
        assert_eq!(
            super::classify_plan_mode_shell_command(command),
            PlanModeShellSafety::Mutating,
            "{command} should be treated as a file-output command"
        );
    }
}

#[test]
fn command_unit_default_is_empty() {
    let unit = CommandUnit::default();
    assert!(unit.name.is_empty());
    assert!(unit.args.is_empty());
    assert!(unit.env.is_empty());
    assert!(unit.redirects.is_empty());
    assert!(!unit.has_substitution);
}

fn test_home() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .expect("HOME or USERPROFILE set in test env")
}

#[test]
fn write_targets_cover_copy_move_install_destination() {
    assert_eq!(
        extract_shell_write_targets("cp secret /etc/passwd"),
        vec!["/etc/passwd".to_string()]
    );
    assert_eq!(
        extract_shell_write_targets("mv build.log /var/log/app.log"),
        vec!["/var/log/app.log".to_string()]
    );
    // `--target-directory` is the destination, not the trailing operand.
    assert_eq!(
        extract_shell_write_targets("cp -t /etc a.conf b.conf"),
        vec!["/etc".to_string()]
    );
}

#[test]
fn write_targets_cover_tee_dd_ln_touch_mkdir() {
    assert_eq!(
        extract_shell_write_targets("echo x | tee /etc/hosts"),
        vec!["/etc/hosts".to_string()]
    );
    assert_eq!(
        extract_shell_write_targets("dd if=/dev/zero of=/boot/blob"),
        vec!["/boot/blob".to_string()]
    );
    // Both operands are reported so an in-workspace symlink pointing at an
    // outside target (`/etc/passwd`) still escalates.
    assert_eq!(
        extract_shell_write_targets("ln -s /etc/passwd /workspace/link"),
        vec!["/etc/passwd".to_string(), "/workspace/link".to_string()]
    );
    assert_eq!(
        extract_shell_write_targets("touch /etc/cron.d/evil"),
        vec!["/etc/cron.d/evil".to_string()]
    );
    assert_eq!(
        extract_shell_write_targets("mkdir /opt/payload"),
        vec!["/opt/payload".to_string()]
    );
}

#[test]
fn write_targets_handle_sed_in_place_and_chmod_mode() {
    // Positional script is dropped; the file operand remains.
    assert_eq!(
        extract_shell_write_targets("sed -i 's/a/b/' /etc/hosts"),
        vec!["/etc/hosts".to_string()]
    );
    // The mode (`777`) is not a path; only the file operand is a target.
    assert_eq!(
        extract_shell_write_targets("chmod 777 /etc/passwd"),
        vec!["/etc/passwd".to_string()]
    );
}

#[test]
fn write_targets_expand_home_prefix() {
    let home = test_home();
    assert_eq!(
        extract_shell_write_targets("sed -i 's/a/b/' ~/.bashrc"),
        vec![format!("{}/.bashrc", home.trim_end_matches('/'))]
    );
}

#[test]
fn write_targets_unwrap_sh_dash_c() {
    assert_eq!(
        extract_shell_write_targets("sh -c \"cp loot /etc/loot\""),
        vec!["/etc/loot".to_string()]
    );
}

#[test]
fn write_targets_ignore_reads_devnull_and_stdin() {
    // Read-only verbs and `/dev/null` redirects produce no write targets.
    assert!(extract_shell_write_targets("grep -R foo .").is_empty());
    assert!(extract_shell_write_targets("cat file | sort").is_empty());
    assert!(extract_shell_write_targets("ls -la 2>/dev/null").is_empty());
    // `cp - dest`/`tee -` stream handles are not paths.
    assert!(extract_shell_write_targets("tee -").is_empty());
}

#[test]
fn write_targets_relative_in_workspace_paths_are_still_reported() {
    // Extraction is path-blind; the caller decides in/out of workspace. A
    // relative destination is reported verbatim so the resolver can place it
    // under the workspace root.
    assert_eq!(
        extract_shell_write_targets("cp a.txt src/b.txt"),
        vec!["src/b.txt".to_string()]
    );
}

#[test]
fn write_targets_expand_env_vars() {
    let home = test_home();
    let home = home.trim_end_matches('/');
    assert_eq!(
        extract_shell_write_targets("touch \"$HOME/abbas.txt\""),
        vec![format!("{home}/abbas.txt")],
        "$HOME must expand so the home write is seen as out-of-workspace"
    );
    assert_eq!(
        extract_shell_write_targets("tee ${HOME}/x"),
        vec![format!("{home}/x")]
    );
    // An unset variable is left literal so the caller escalates on the `$`.
    assert_eq!(
        extract_shell_write_targets("touch $SQZ_DEFINITELY_UNSET_VAR/x"),
        vec!["$SQZ_DEFINITELY_UNSET_VAR/x".to_string()]
    );
}

#[test]
fn write_targets_cover_windows_verbs() {
    // cmd copy/move/xcopy: destination is the last non-switch operand
    // (forward-slash + quoted forms keep the bash tokenizer unambiguous).
    assert_eq!(
        extract_shell_write_targets("copy secret \"C:/Windows/evil.txt\""),
        vec!["C:/Windows/evil.txt".to_string()]
    );
    assert_eq!(
        extract_shell_write_targets("xcopy src \"D:/out\" /E /I"),
        vec!["D:/out".to_string()]
    );
    // robocopy: destination is the 2nd positional.
    assert_eq!(
        extract_shell_write_targets("robocopy src \"//server/share\" /MIR"),
        vec!["//server/share".to_string()]
    );
    // md = cmd mkdir alias.
    assert_eq!(
        extract_shell_write_targets("md \"E:/payload\""),
        vec!["E:/payload".to_string()]
    );
    // PowerShell named destination + file writers.
    assert_eq!(
        extract_shell_write_targets("Copy-Item secret -Destination \"C:/Windows/x\""),
        vec!["C:/Windows/x".to_string()]
    );
    assert_eq!(
        extract_shell_write_targets("Set-Content -Path \"C:/hosts\" -Value y"),
        vec!["C:/hosts".to_string()]
    );
    assert_eq!(
        extract_shell_write_targets("Out-File \"C:/log.txt\""),
        vec!["C:/log.txt".to_string()]
    );
}

#[test]
fn expand_env_vars_does_not_panic_on_multi_byte_brace_content() {
    // Regression: `powershell_env_provider_var` previously sliced the brace
    // body at byte index 4 with `split_at`, which panicked when byte 4 fell
    // inside a multi-byte UTF-8 codepoint (e.g. `ℓ`). The expander is reached
    // from any user-typed shell command via `extract_shell_write_targets` →
    // `expand_path_vars` → `expand_env_vars`, so a panic there crashes the
    // shell tool pipeline. The non-`env:` brace body must round-trip as a
    // literal `${...}`.
    let target = "touch \"${ñℓ:VAR}/x\"";
    let extracted = extract_shell_write_targets(target);
    assert_eq!(extracted, vec!["${ñℓ:VAR}/x".to_string()]);
    // Same shape with the `${env:` prefix straddled by a multi-byte char must
    // also round-trip literally — the prefix check is byte-aligned and the
    // body is not `env:`.
    let extracted = extract_shell_write_targets("touch \"${€nv:VAR}/x\"");
    assert_eq!(extracted, vec!["${€nv:VAR}/x".to_string()]);
}

#[test]
fn write_targets_expand_powershell_env_provider() {
    let home = test_home();
    let home = home.trim_end_matches('/');
    // `$env:HOME` (case-insensitive `env:`) resolves through the PowerShell
    // env-provider syntax the same as `$HOME`.
    assert_eq!(
        extract_shell_write_targets("Out-File \"$env:HOME/x\""),
        vec![format!("{home}/x")]
    );
    assert_eq!(
        extract_shell_write_targets("Out-File \"$ENV:HOME/x\""),
        vec![format!("{home}/x")]
    );
    // Brace form `${env:HOME}` resolves identically.
    assert_eq!(
        extract_shell_write_targets("Out-File \"${env:HOME}/x\""),
        vec![format!("{home}/x")]
    );
    // Unset env-provider name stays literal so the unresolved-var check
    // escalates on the residual `$`.
    assert_eq!(
        extract_shell_write_targets("Out-File \"$env:SQZ_DEFINITELY_UNSET/x\""),
        vec!["$env:SQZ_DEFINITELY_UNSET/x".to_string()]
    );
    assert_eq!(
        extract_shell_write_targets("Out-File \"${env:SQZ_DEFINITELY_UNSET}/x\""),
        vec!["${env:SQZ_DEFINITELY_UNSET}/x".to_string()]
    );
    // Degenerate `$env:` with no name is left literal.
    assert_eq!(
        extract_shell_write_targets("Out-File \"$env:/x\""),
        vec!["$env:/x".to_string()]
    );
}

#[test]
fn write_targets_expand_percent_vars() {
    let home = test_home();
    let home = home.trim_end_matches('/');
    // %HOME% is set in the test env; resolves like cmd's %USERPROFILE%.
    assert_eq!(
        extract_shell_write_targets("copy secret \"%HOME%/evil.txt\""),
        vec![format!("{home}/evil.txt")]
    );
    // An unset %VAR% stays literal so the escape check escalates on it.
    assert_eq!(
        extract_shell_write_targets("copy secret \"%SQZ_UNSET_VAR%/x\""),
        vec!["%SQZ_UNSET_VAR%/x".to_string()]
    );
}

#[test]
fn write_targets_preserve_unquoted_backslash_windows_path() {
    // The bash tokenizer preserves an unquoted backslash drive path verbatim,
    // so on a Windows build std::path resolves `C:\...` as an absolute
    // (drive-prefixed) path and the workspace-escape check flags it.
    assert_eq!(
        extract_shell_write_targets("copy secret C:\\Windows\\evil.txt"),
        vec!["C:\\Windows\\evil.txt".to_string()]
    );
}
