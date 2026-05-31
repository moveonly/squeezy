use super::{DispatchCommand, DispatchCommandParseError};

fn parse(input: &str) -> Result<DispatchCommand, DispatchCommandParseError> {
    DispatchCommand::parse(input)
}

#[test]
fn parse_help_with_and_without_topic() {
    assert_eq!(
        parse("/help").unwrap(),
        DispatchCommand::Help { topic: None }
    );
    assert_eq!(
        parse("/help quantum billing").unwrap(),
        DispatchCommand::Help {
            topic: Some("quantum billing".to_string())
        }
    );
}

#[test]
fn parse_config_section_arg() {
    assert_eq!(
        parse("/config").unwrap(),
        DispatchCommand::Config { section: None }
    );
    assert_eq!(
        parse("/config models").unwrap(),
        DispatchCommand::Config {
            section: Some("models".to_string())
        }
    );
    assert_eq!(
        parse("/options permissions").unwrap(),
        DispatchCommand::Config {
            section: Some("permissions".to_string())
        },
        "/options remains a hidden compatibility alias for /config"
    );
    assert_eq!(
        parse("/options").unwrap().slash_name(),
        "/config",
        "/config is the canonical command surfaced to users"
    );
}

#[test]
fn parse_model_and_permissions() {
    assert_eq!(parse("/model").unwrap(), DispatchCommand::Model);
    assert_eq!(parse("/permissions").unwrap(), DispatchCommand::Permissions);
}

#[test]
fn parse_plan_and_build_capture_inline_prompt() {
    assert_eq!(
        parse("/plan").unwrap(),
        DispatchCommand::Plan { prompt: None }
    );
    assert_eq!(
        parse("/plan refactor X").unwrap(),
        DispatchCommand::Plan {
            prompt: Some("refactor X".to_string())
        }
    );
    assert_eq!(
        parse("/build apply that plan").unwrap(),
        DispatchCommand::Build {
            prompt: Some("apply that plan".to_string())
        }
    );
}

#[test]
fn parse_plans_passes_subcommand_through() {
    assert_eq!(
        parse("/plans").unwrap(),
        DispatchCommand::Plans {
            args: String::new()
        }
    );
    assert_eq!(
        parse("/plans show p123").unwrap(),
        DispatchCommand::Plans {
            args: "show p123".to_string()
        }
    );
}

#[test]
fn parse_cost_context_reviewer() {
    assert_eq!(parse("/cost").unwrap(), DispatchCommand::Cost);
    assert_eq!(parse("/context").unwrap(), DispatchCommand::Context);
    assert_eq!(parse("/reviewer").unwrap(), DispatchCommand::Reviewer);
}

#[test]
fn parse_attach_requires_path() {
    let err = parse("/attach").unwrap_err();
    assert!(matches!(err, DispatchCommandParseError::Usage { .. }));
    assert_eq!(
        parse("/attach src/lib.rs").unwrap(),
        DispatchCommand::Attach {
            path: "src/lib.rs".to_string()
        }
    );
}

#[test]
fn parse_attachments() {
    assert_eq!(parse("/attachments").unwrap(), DispatchCommand::Attachments);
}

#[test]
fn parse_copy_is_not_a_builtin_slash_command() {
    let err = parse("/copy").unwrap_err();
    assert!(matches!(err, DispatchCommandParseError::Unknown { .. }));
}

#[test]
fn parse_compact_undo_flag() {
    assert_eq!(
        parse("/compact").unwrap(),
        DispatchCommand::Compact { undo: false }
    );
    assert_eq!(
        parse("/compact undo").unwrap(),
        DispatchCommand::Compact { undo: true }
    );
    assert_eq!(
        parse("/compact UNDO").unwrap(),
        DispatchCommand::Compact { undo: true }
    );
}

#[test]
fn parse_diff_keymap_statusline_no_args() {
    assert_eq!(parse("/diff").unwrap(), DispatchCommand::Diff);
    assert_eq!(parse("/keymap").unwrap(), DispatchCommand::Keymap);
    assert_eq!(parse("/statusline").unwrap(), DispatchCommand::Statusline);
}

#[test]
fn parse_task_family_requires_id() {
    assert_eq!(parse("/tasks").unwrap(), DispatchCommand::Tasks);
    let err = parse("/task").unwrap_err();
    assert!(matches!(err, DispatchCommandParseError::Usage { .. }));
    assert_eq!(
        parse("/task 7").unwrap(),
        DispatchCommand::Task {
            id: "7".to_string()
        }
    );
    assert_eq!(
        parse("/task-cancel 7").unwrap(),
        DispatchCommand::TaskCancel {
            id: "7".to_string()
        }
    );
}

#[test]
fn parse_jobs_family_no_longer_recognized() {
    assert!(matches!(
        parse("/jobs").unwrap_err(),
        DispatchCommandParseError::Unknown { .. }
    ));
    assert!(matches!(
        parse("/job 12").unwrap_err(),
        DispatchCommandParseError::Unknown { .. }
    ));
    assert!(matches!(
        parse("/job-cancel 12").unwrap_err(),
        DispatchCommandParseError::Unknown { .. }
    ));
}

#[test]
fn parse_pin_family() {
    assert_eq!(parse("/pins").unwrap(), DispatchCommand::Pins);
    assert_eq!(
        parse("/pin").unwrap(),
        DispatchCommand::Pin { target: None }
    );
    assert_eq!(
        parse("/pin last").unwrap(),
        DispatchCommand::Pin {
            target: Some("last".to_string())
        }
    );
    assert!(matches!(
        parse("/unpin").unwrap_err(),
        DispatchCommandParseError::Usage { .. }
    ));
    assert_eq!(
        parse("/unpin pin-1").unwrap(),
        DispatchCommand::Unpin {
            id: "pin-1".to_string()
        }
    );
}

#[test]
fn parse_feedback_and_report_pass_args() {
    assert_eq!(
        parse("/feedback").unwrap(),
        DispatchCommand::Feedback {
            args: String::new()
        }
    );
    assert_eq!(
        parse("/feedback something broke").unwrap(),
        DispatchCommand::Feedback {
            args: "something broke".to_string()
        }
    );
    assert_eq!(
        parse("/report send").unwrap(),
        DispatchCommand::Report {
            args: "send".to_string()
        }
    );
}

#[test]
fn parse_session_family() {
    assert_eq!(parse("/sessions").unwrap(), DispatchCommand::Sessions);
    assert!(matches!(
        parse("/session").unwrap_err(),
        DispatchCommandParseError::Usage { .. }
    ));
    assert_eq!(
        parse("/session sess-1").unwrap(),
        DispatchCommand::Session {
            id: "sess-1".to_string()
        }
    );
    // `/session rename <name>` captures the remainder verbatim so a
    // user can pass multi-word names without quoting.
    assert_eq!(
        parse("/session rename payments refactor").unwrap(),
        DispatchCommand::SessionRename {
            name: "payments refactor".to_string()
        }
    );
    // `/session rename` with no argument clears the display_name.
    assert_eq!(
        parse("/session rename").unwrap(),
        DispatchCommand::SessionRename {
            name: String::new()
        }
    );
    assert_eq!(
        parse("/session label bugfix").unwrap(),
        DispatchCommand::SessionLabel {
            name: "bugfix".to_string()
        }
    );
    // `/session label` without an argument is a usage error — labels
    // are append-only so an empty argument has no useful meaning.
    assert!(matches!(
        parse("/session label").unwrap_err(),
        DispatchCommandParseError::Usage { .. }
    ));
    // Reserved subcommands take precedence over treating the head as
    // a session id; both `rename` and `label` are unrelated to real
    // session ids (timestamped hex slugs) so this is unambiguous.
    assert_eq!(parse("/session rename").unwrap().slash_name(), "/session");
    assert_eq!(
        parse("/session label tag").unwrap().slash_name(),
        "/session"
    );
    assert_eq!(
        parse("/resume sess-2").unwrap(),
        DispatchCommand::Resume {
            id: "sess-2".to_string()
        }
    );
    assert_eq!(parse("/fork").unwrap(), DispatchCommand::Fork);
    assert_eq!(
        parse("/session-export sess-3").unwrap(),
        DispatchCommand::SessionExport {
            id: "sess-3".to_string()
        }
    );
    assert_eq!(
        parse("/session-export-html sess-4").unwrap(),
        DispatchCommand::SessionExportHtml {
            id: "sess-4".to_string(),
            path: None,
        }
    );
    assert_eq!(
        parse("/session-export-html sess-4 /tmp/x.html").unwrap(),
        DispatchCommand::SessionExportHtml {
            id: "sess-4".to_string(),
            path: Some("/tmp/x.html".to_string()),
        }
    );
    assert_eq!(
        parse("/session-cleanup --archive a b").unwrap(),
        DispatchCommand::SessionCleanup {
            args: "--archive a b".to_string()
        }
    );
}

#[test]
fn parse_checkpoint_family() {
    assert_eq!(parse("/checkpoints").unwrap(), DispatchCommand::Checkpoints);
    assert!(matches!(
        parse("/checkpoint").unwrap_err(),
        DispatchCommandParseError::Usage { .. }
    ));
    assert_eq!(
        parse("/checkpoint ck-1").unwrap(),
        DispatchCommand::Checkpoint {
            id: "ck-1".to_string()
        }
    );
    assert_eq!(parse("/undo").unwrap(), DispatchCommand::Undo);
    assert_eq!(
        parse("/revert-turn turn-7").unwrap(),
        DispatchCommand::RevertTurn {
            group_id: "turn-7".to_string()
        }
    );
}

#[test]
fn parse_verbosity_effort_theme_detach_keymap() {
    assert_eq!(
        parse("/effort").unwrap(),
        DispatchCommand::Effort { value: None }
    );
    assert_eq!(
        parse("/effort high").unwrap(),
        DispatchCommand::Effort {
            value: Some("high".to_string())
        }
    );
    assert_eq!(
        parse("/verbosity verbose").unwrap(),
        DispatchCommand::Verbosity {
            value: Some("verbose".to_string())
        }
    );
    assert_eq!(
        parse("/tool-verbosity compact").unwrap(),
        DispatchCommand::ToolVerbosity {
            value: Some("compact".to_string())
        }
    );
    assert!(matches!(
        parse("/detach").unwrap_err(),
        DispatchCommandParseError::Usage { .. }
    ));
    assert_eq!(
        parse("/detach att-1").unwrap(),
        DispatchCommand::Detach {
            id: "att-1".to_string()
        }
    );
    assert_eq!(
        parse("/theme").unwrap(),
        DispatchCommand::Theme { theme: None }
    );
    assert_eq!(
        parse("/theme dark").unwrap(),
        DispatchCommand::Theme {
            theme: Some("dark".to_string())
        }
    );
}

#[test]
fn parse_unknown_returns_typed_error() {
    let err = parse("/no-such-command").unwrap_err();
    assert!(matches!(
        err,
        DispatchCommandParseError::Unknown { ref command }
            if command == "/no-such-command"
    ));
}

#[test]
fn parse_non_slash_returns_typed_error() {
    assert!(matches!(
        parse("hello").unwrap_err(),
        DispatchCommandParseError::NotASlashCommand
    ));
}

#[test]
fn slash_name_matches_input_command() {
    let cases = &[
        ("/help", DispatchCommand::Help { topic: None }),
        ("/cost", DispatchCommand::Cost),
        ("/diff", DispatchCommand::Diff),
        ("/compact", DispatchCommand::Compact { undo: false }),
        (
            "/theme",
            DispatchCommand::Theme {
                theme: Some("dark".to_string()),
            },
        ),
        (
            "/task-cancel",
            DispatchCommand::TaskCancel {
                id: "1".to_string(),
            },
        ),
        (
            "/session-export-html",
            DispatchCommand::SessionExportHtml {
                id: "x".into(),
                path: None,
            },
        ),
        (
            "/revert-turn",
            DispatchCommand::RevertTurn {
                group_id: "t".into(),
            },
        ),
    ];
    for (expected, cmd) in cases {
        assert_eq!(cmd.slash_name(), *expected);
    }
}
