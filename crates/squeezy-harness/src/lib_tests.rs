use super::*;
use squeezy_llm::{LlmInputItem, LlmToolCall};

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
fn path_matches_requires_segment_boundary_for_literal_patterns() {
    assert!(path_matches("*.rs", "src/lib.rs"));
    assert!(path_matches("lib.rs", "src/lib.rs"));
    assert!(path_matches("src/main.rs", "src/main.rs"));
    assert!(path_matches("/src/main.rs", "src/main.rs"));
    // Multi-segment patterns still match at any directory depth, but only
    // when the suffix lands on a directory boundary.
    assert!(path_matches("src/main.rs", "tests/src/main.rs"));
    // The boundary check rejects mid-filename suffix matches.
    assert!(!path_matches("lib.rs", "src/sublib.rs"));
    assert!(!path_matches("lib.rs", "vendor/anylib.rs"));
    assert!(!path_matches("main.rs", "src/zmain.rs"));
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

#[tokio::test]
async fn agent_runner_scopes_tools_to_materialized_workspace_and_counts_tool_cost() {
    let suffix = unique_suffix();
    let path = format!("src/generated-{suffix}.rs");
    let marker = format!("harness_marker_{suffix}");
    let task = TaskSpec {
        id: "tool-workspace".to_string(),
        title: "Tool workspace".to_string(),
        prompt: "Find the generated marker".to_string(),
        workspace: WorkspaceSpec {
            files: vec![WorkspaceFile {
                path: path.clone(),
                content: format!("pub fn generated() {{ /* {marker} */ }}\n"),
            }],
        },
        expect: ExpectSpec {
            contains: vec![path.clone()],
        },
        mock: None,
        baseline: None,
    };
    let provider = Arc::new(ToolUsingProvider::new(marker, path.clone()));
    let config = AppConfig {
        workspace_root: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        ..Default::default()
    };

    let output = run_agent_with_config(&task, RunnerKind::MockOpenai, provider, config)
        .await
        .expect("agent run");

    assert!(output.final_answer.contains(&path));
    assert_eq!(output.metrics.tool_calls, 1);
    assert!(output.metrics.files_scanned >= 1);
    assert!(output.metrics.bytes_read > 0);
    assert_eq!(output.metrics.matches_returned, 1);
}

#[derive(Debug)]
struct ToolUsingProvider {
    marker: String,
    path: String,
}

impl ToolUsingProvider {
    fn new(marker: String, path: String) -> Self {
        Self { marker, path }
    }
}

impl LlmProvider for ToolUsingProvider {
    fn name(&self) -> &'static str {
        "tool-using-provider"
    }

    fn stream_response(&self, request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        let tool_output = request.input.iter().find_map(|item| match item {
            LlmInputItem::FunctionCallOutput { output, .. } => Some(output.as_str()),
            _ => None,
        });
        let events = if let Some(output) = tool_output {
            let answer = if output.contains(&self.path)
                && output.contains(&self.marker)
                && output.contains("\"status\":\"Success\"")
            {
                format!("found {}", self.path)
            } else {
                format!("missing fixture output: {output}")
            };
            vec![
                Ok(LlmEvent::TextDelta(answer)),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_2".to_string()),
                    cost: CostSnapshot::default(),
                }),
            ]
        } else {
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::ToolCall(LlmToolCall {
                    call_id: "grep_1".to_string(),
                    name: "grep".to_string(),
                    arguments: json!({
                        "pattern": self.marker,
                        "include": ["*.rs"],
                        "output_mode": "content"
                    }),
                })),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_1".to_string()),
                    cost: CostSnapshot::default(),
                }),
            ]
        };
        Box::pin(stream::iter(events))
    }
}
