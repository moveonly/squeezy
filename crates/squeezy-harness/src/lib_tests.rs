use super::*;

#[test]
fn parses_task_toml_with_provider_traces() {
    let task: TaskSpec = toml::from_str(
        r#"
id = "find-symbol"
title = "Find symbol"
prompt = "Where is make_widget?"

[expect]
contains = ["src/lib.rs"]

[[workspace.files]]
path = "src/lib.rs"
content = "pub fn make_widget() {}\n"

[mock.openai]
[[mock.openai.events]]
kind = "started"
[[mock.openai.events]]
kind = "text_delta"
text = "src/lib.rs"
[[mock.openai.events]]
kind = "completed"
input_tokens = 10
output_tokens = 4

[mock.anthropic]
[[mock.anthropic.events]]
kind = "started"
[[mock.anthropic.events]]
kind = "text_delta"
text = "src/lib.rs"
[[mock.anthropic.events]]
kind = "completed"
input_tokens = 11
output_tokens = 5

[baseline]
pattern = "make_widget"
include = ["*.rs"]
mode = "paths"
"#,
    )
    .expect("task toml");

    assert_eq!(task.id, "find-symbol");
    assert_eq!(mock_events(&task, "openai").unwrap().len(), 3);
    assert_eq!(mock_events(&task, "anthropic").unwrap().len(), 3);
}

#[test]
fn rejects_workspace_paths_outside_root() {
    let escaping_path = ["..", "escape.txt"].join("/");
    let err = safe_relative_path(&escaping_path).expect_err("path should be rejected");

    assert!(err.to_string().contains("relative"));
}

#[test]
fn baseline_paths_skips_ignored_directories() {
    let task = TaskSpec {
        id: "ignore".to_string(),
        title: "Ignore generated".to_string(),
        prompt: "Find needle".to_string(),
        workspace: WorkspaceSpec {
            files: vec![
                WorkspaceFile {
                    path: "src/lib.rs".to_string(),
                    content: "pub fn needle() {}\n".to_string(),
                },
                WorkspaceFile {
                    path: "vendor/lib.rs".to_string(),
                    content: "pub fn needle() {}\n".to_string(),
                },
            ],
        },
        expect: ExpectSpec {
            contains: vec!["src/lib.rs".to_string()],
        },
        mock: None,
        baseline: Some(BaselineSpec {
            pattern: "needle".to_string(),
            include: vec!["*.rs".to_string()],
            mode: BaselineMode::Paths,
            read_path: None,
        }),
    };
    let output = run_baseline(&task).expect("baseline");

    assert!(output.final_answer.contains("src/lib.rs"));
    assert!(!output.final_answer.contains("vendor/lib.rs"));
    assert_eq!(output.metrics.matches_returned, 1);
}

#[tokio::test]
async fn mock_runner_uses_trace_events_and_scores_correctness() {
    let task = TaskSpec {
        id: "mock".to_string(),
        title: "Mock".to_string(),
        prompt: "answer".to_string(),
        workspace: WorkspaceSpec { files: Vec::new() },
        expect: ExpectSpec {
            contains: vec!["done".to_string()],
        },
        mock: Some(MockSpec {
            openai: Some(MockProviderSpec {
                events: vec![
                    TraceEvent {
                        kind: TraceEventKind::Started,
                        text: None,
                        response_id: None,
                        input_tokens: None,
                        output_tokens: None,
                        cached_input_tokens: None,
                    },
                    TraceEvent {
                        kind: TraceEventKind::TextDelta,
                        text: Some("done".to_string()),
                        response_id: None,
                        input_tokens: None,
                        output_tokens: None,
                        cached_input_tokens: None,
                    },
                    TraceEvent {
                        kind: TraceEventKind::Completed,
                        text: None,
                        response_id: Some("resp".to_string()),
                        input_tokens: Some(3),
                        output_tokens: Some(1),
                        cached_input_tokens: None,
                    },
                ],
            }),
            anthropic: None,
        }),
        baseline: None,
    };

    let result = run_task(&task, RunnerKind::MockOpenai, None).await;

    assert_eq!(result.status, TaskStatus::Passed);
    assert_eq!(result.metrics.input_tokens, Some(3));
    assert_eq!(result.metrics.output_tokens, Some(1));
}
