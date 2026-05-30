//! Unit tests for the typed `CommandUnit` payload produced by
//! `extract_command_units`. A structured walk should surface
//! `{ name, args, env, redirects, has_substitution }` per `command` node
//! so downstream classifiers can stop re-splitting segment text via
//! `split_whitespace`.

use super::{CommandUnit, Redirect, extract_command_units};

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
fn command_unit_default_is_empty() {
    let unit = CommandUnit::default();
    assert!(unit.name.is_empty());
    assert!(unit.args.is_empty());
    assert!(unit.env.is_empty());
    assert!(unit.redirects.is_empty());
    assert!(!unit.has_substitution);
}
