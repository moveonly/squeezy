use super::*;

#[test]
fn parses_minimal_scenario() {
    let toml = r#"
id = "smoke"
title = "Smoke test"

[workspace]
local = "/tmp/repo"

[[steps]]
kind = "prompt"
text = "hello?"
"#;
    let scenario: Scenario = toml::from_str(toml).unwrap();
    assert_eq!(scenario.id, "smoke");
    assert_eq!(scenario.steps.len(), 1);
    match &scenario.workspace {
        WorkspaceSpec::Local { path } => assert_eq!(path, &PathBuf::from("/tmp/repo")),
        other => panic!("expected local workspace, got {other:?}"),
    }
}

#[test]
fn parses_action_step() {
    let toml = r#"
id = "approve"
title = "Approve test"

[workspace]
local = "/tmp/repo"

[[steps]]
kind = "action"
action = "approve"

[steps.match]
tool = "fs.write"
"#;
    let scenario: Scenario = toml::from_str(toml).unwrap();
    match &scenario.steps[0] {
        Step::Action(Action::Approve { r#match, .. }) => {
            assert_eq!(r#match.as_ref().unwrap().tool.as_deref(), Some("fs.write"));
        }
        other => panic!("expected approve action, got {other:?}"),
    }
}

#[test]
fn parses_github_workspace() {
    let toml = r#"
id = "gh"
title = "GH"

[workspace.github]
repo = "esqueezy/squeezy-fixture"
sha = "deadbeef"
"#;
    let scenario: Scenario = toml::from_str(toml).unwrap();
    match scenario.workspace {
        WorkspaceSpec::Github { github } => {
            assert_eq!(github.repo, "esqueezy/squeezy-fixture");
            assert_eq!(github.sha, "deadbeef");
        }
        other => panic!("expected github workspace, got {other:?}"),
    }
}

#[test]
fn rejects_edit_file_without_payload() {
    let toml = r#"
id = "bad"
title = "bad"

[workspace]
local = "/tmp/repo"

[[steps]]
kind = "action"
action = "edit_file"
path = "x"
"#;
    let scenario: Scenario = toml::from_str(toml).unwrap();
    assert!(scenario.validate().is_err());
}
