use std::{
    collections::{BTreeSet, VecDeque},
    fmt,
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use clap::ValueEnum;
use futures_core::Stream;
use futures_util::stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use squeezy_agent::{Agent, AgentEvent};
use squeezy_core::{
    AppConfig, CostSnapshot, DEFAULT_ANTHROPIC_MODEL, DEFAULT_AZURE_OPENAI_MODEL,
    DEFAULT_BEDROCK_MODEL, DEFAULT_GOOGLE_MODEL, DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_OLLAMA_MODEL,
    DEFAULT_OPENAI_MODEL, Result, SqueezyError,
};
use squeezy_llm::{
    LlmEvent, LlmProvider, LlmRequest, LlmStream, provider_from_config as llm_provider_from_config,
};
use tokio_util::sync::CancellationToken;

const COSTLY_FLAG: &str = "SQUEEZY_RUN_COSTLY_TESTS";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum RunnerKind {
    MockOpenai,
    MockAnthropic,
    GrepBaseline,
    CostlyOpenai,
    CostlyAnthropic,
    CostlyGoogle,
    CostlyAzureOpenai,
    CostlyOllama,
    CostlyBedrock,
}

impl RunnerKind {
    pub const fn is_costly(self) -> bool {
        matches!(
            self,
            Self::CostlyOpenai
                | Self::CostlyAnthropic
                | Self::CostlyGoogle
                | Self::CostlyAzureOpenai
                | Self::CostlyOllama
                | Self::CostlyBedrock
        )
    }

    pub const fn is_mock(self) -> bool {
        matches!(self, Self::MockOpenai | Self::MockAnthropic)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::MockOpenai => "mock-openai",
            Self::MockAnthropic => "mock-anthropic",
            Self::GrepBaseline => "grep-baseline",
            Self::CostlyOpenai => "costly-openai",
            Self::CostlyAnthropic => "costly-anthropic",
            Self::CostlyGoogle => "costly-google",
            Self::CostlyAzureOpenai => "costly-azure-openai",
            Self::CostlyOllama => "costly-ollama",
            Self::CostlyBedrock => "costly-bedrock",
        }
    }
}

impl fmt::Display for RunnerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

pub fn default_tasks_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/tasks")
}

pub fn default_runners() -> Vec<RunnerKind> {
    vec![
        RunnerKind::MockOpenai,
        RunnerKind::MockAnthropic,
        RunnerKind::GrepBaseline,
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub id: String,
    pub title: String,
    pub prompt: String,
    pub workspace: WorkspaceSpec,
    pub expect: ExpectSpec,
    pub mock: Option<MockSpec>,
    pub baseline: Option<BaselineSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSpec {
    pub files: Vec<WorkspaceFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExpectSpec {
    #[serde(default)]
    pub contains: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MockSpec {
    pub openai: Option<MockProviderSpec>,
    pub anthropic: Option<MockProviderSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockProviderSpec {
    pub events: Vec<TraceEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEvent {
    pub kind: TraceEventKind,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub response_id: Option<String>,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cached_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceEventKind {
    Started,
    TextDelta,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineSpec {
    pub pattern: String,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default = "default_baseline_mode")]
    pub mode: BaselineMode,
    #[serde(default)]
    pub read_path: Option<String>,
}

fn default_baseline_mode() -> BaselineMode {
    BaselineMode::Paths
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaselineMode {
    Paths,
    Count,
    FirstLine,
    Read,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessMetrics {
    pub wall_ms: u128,
    pub tool_calls: u64,
    pub tool_successes: u64,
    pub tool_errors: u64,
    pub tool_denials: u64,
    pub tool_cancellations: u64,
    pub files_scanned: u64,
    pub bytes_read: u64,
    pub matches_returned: u64,
    pub output_bytes: u64,
    pub receipt_stub_hits: u64,
    pub negative_receipt_hits: u64,
    pub spill_writes: u64,
    pub spill_reads: u64,
    pub budget_denials: u64,
    pub redactions: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub estimated_usd_micros: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Correctness {
    pub passed: bool,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Passed,
    Failed,
    Error,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub task_id: String,
    pub title: String,
    pub runner: RunnerKind,
    pub status: TaskStatus,
    pub correctness: Correctness,
    pub metrics: HarnessMetrics,
    pub final_answer: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceRecord {
    pub task_id: String,
    pub runner: RunnerKind,
    pub events: Vec<TraceEvent>,
}

#[derive(Debug, Clone)]
pub struct HarnessConfig {
    pub tasks_dir: PathBuf,
    pub runners: Vec<RunnerKind>,
    pub jsonl_path: Option<PathBuf>,
    pub trace_dir: Option<PathBuf>,
}

pub fn load_tasks(tasks_dir: &Path) -> Result<Vec<TaskSpec>> {
    let mut paths = fs::read_dir(tasks_dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.retain(|path| path.extension().is_some_and(|ext| ext == "toml"));
    paths.sort();

    let mut tasks = Vec::new();
    for path in paths {
        let content = fs::read_to_string(&path)?;
        let task = toml::from_str::<TaskSpec>(&content).map_err(|err| {
            SqueezyError::Agent(format!("failed to parse {}: {err}", path.display()))
        })?;
        validate_task(&task)?;
        tasks.push(task);
    }
    Ok(tasks)
}

pub async fn run_harness(config: HarnessConfig) -> Result<Vec<TaskResult>> {
    let tasks = load_tasks(&config.tasks_dir)?;
    let runners = if config.runners.is_empty() {
        default_runners()
    } else {
        config.runners
    };
    let mut results = Vec::new();

    for task in &tasks {
        for runner in &runners {
            let result = run_task(task, *runner, config.trace_dir.as_deref()).await;
            results.push(result);
        }
    }

    if let Some(path) = &config.jsonl_path {
        write_jsonl(path, &results)?;
    }

    Ok(results)
}

pub async fn run_task(task: &TaskSpec, runner: RunnerKind, trace_dir: Option<&Path>) -> TaskResult {
    let started = Instant::now();
    let outcome = match runner {
        RunnerKind::MockOpenai => run_mock(task, runner, mock_events(task, "openai")).await,
        RunnerKind::MockAnthropic => run_mock(task, runner, mock_events(task, "anthropic")).await,
        RunnerKind::GrepBaseline => run_baseline(task),
        RunnerKind::CostlyOpenai => run_costly(task, runner, "openai", trace_dir).await,
        RunnerKind::CostlyAnthropic => run_costly(task, runner, "anthropic", trace_dir).await,
        RunnerKind::CostlyGoogle => run_costly(task, runner, "google", trace_dir).await,
        RunnerKind::CostlyAzureOpenai => run_costly(task, runner, "azure_openai", trace_dir).await,
        RunnerKind::CostlyOllama => run_costly(task, runner, "ollama", trace_dir).await,
        RunnerKind::CostlyBedrock => run_costly(task, runner, "bedrock", trace_dir).await,
    };

    match outcome {
        Ok(mut output) => {
            output.metrics.wall_ms = started.elapsed().as_millis();
            let correctness = evaluate(task, &output.final_answer);
            let status = if correctness.passed {
                TaskStatus::Passed
            } else {
                TaskStatus::Failed
            };
            TaskResult {
                task_id: task.id.clone(),
                title: task.title.clone(),
                runner,
                status,
                correctness,
                metrics: output.metrics,
                final_answer: output.final_answer,
                error: None,
            }
        }
        Err(error) => TaskResult {
            task_id: task.id.clone(),
            title: task.title.clone(),
            runner,
            status: if runner.is_costly() {
                TaskStatus::Skipped
            } else {
                TaskStatus::Error
            },
            correctness: Correctness {
                passed: false,
                reasons: vec![error.to_string()],
            },
            metrics: HarnessMetrics {
                wall_ms: started.elapsed().as_millis(),
                ..HarnessMetrics::default()
            },
            final_answer: String::new(),
            error: Some(error.to_string()),
        },
    }
}

struct RunnerOutput {
    final_answer: String,
    metrics: HarnessMetrics,
    trace: Vec<TraceEvent>,
}

async fn run_mock(
    task: &TaskSpec,
    runner: RunnerKind,
    events: Result<Vec<TraceEvent>>,
) -> Result<RunnerOutput> {
    let events = events?;
    let provider = Arc::new(ScriptedProvider::new(runner.name(), events));
    run_agent(task, runner, provider).await
}

async fn run_costly(
    task: &TaskSpec,
    runner: RunnerKind,
    provider_name: &str,
    trace_dir: Option<&Path>,
) -> Result<RunnerOutput> {
    require_costly(provider_name)?;
    let mut config = AppConfig::from_env_with_provider(provider_name);
    config.model = costly_model(provider_name);
    config.max_output_tokens = Some(costly_max_output_tokens()?);

    let provider = provider_from_config(&config)?;
    let output = run_agent_with_config(task, runner, provider, config).await?;
    if let Some(trace_dir) = trace_dir {
        write_trace(trace_dir, task, runner, &output.trace)?;
    }
    Ok(output)
}

fn run_baseline(task: &TaskSpec) -> Result<RunnerOutput> {
    let baseline = task
        .baseline
        .as_ref()
        .ok_or_else(|| SqueezyError::Agent(format!("task {} has no baseline", task.id)))?;
    let root = materialize_workspace(task)?;
    let result = run_baseline_in_workspace(&root, baseline);
    let _ = fs::remove_dir_all(&root);
    result
}

fn run_baseline_in_workspace(root: &Path, baseline: &BaselineSpec) -> Result<RunnerOutput> {
    let mut metrics = HarnessMetrics::default();
    let mut matches = Vec::new();
    let mut matched_paths = BTreeSet::new();

    for path in list_files(root)? {
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let relative_text = relative.to_string_lossy().replace('\\', "/");
        if !baseline.include.is_empty()
            && !baseline
                .include
                .iter()
                .any(|pattern| path_matches(pattern, &relative_text))
        {
            continue;
        }
        metrics.files_scanned += 1;
        let content = fs::read_to_string(&path)?;
        metrics.bytes_read += content.len() as u64;
        for (line_index, line) in content.lines().enumerate() {
            if line.contains(&baseline.pattern) {
                metrics.matches_returned += 1;
                matched_paths.insert(relative_text.clone());
                matches.push((relative_text.clone(), line_index + 1, line.to_string()));
            }
        }
    }

    let final_answer = match baseline.mode {
        BaselineMode::Paths => format!(
            "Matched files: {}",
            matched_paths.into_iter().collect::<Vec<_>>().join(", ")
        ),
        BaselineMode::Count => format!("Total matches: {}", metrics.matches_returned),
        BaselineMode::FirstLine => matches
            .first()
            .map(|(path, line, text)| format!("First match: {path}:{line}: {text}"))
            .unwrap_or_else(|| "First match: none".to_string()),
        BaselineMode::Read => {
            let read_path = baseline.read_path.as_deref().ok_or_else(|| {
                SqueezyError::Agent("baseline read mode requires read_path".to_string())
            })?;
            let safe_path = safe_relative_path(read_path)?;
            let content = fs::read_to_string(root.join(safe_path))?;
            metrics.bytes_read += content.len() as u64;
            content
        }
    };
    metrics.output_bytes = final_answer.len() as u64;

    Ok(RunnerOutput {
        final_answer,
        metrics,
        trace: Vec::new(),
    })
}

async fn run_agent(
    task: &TaskSpec,
    runner: RunnerKind,
    provider: Arc<dyn LlmProvider>,
) -> Result<RunnerOutput> {
    let mut config = AppConfig::from_env();
    config.model = runner.name().to_string();
    config.max_output_tokens = Some(DEFAULT_MAX_OUTPUT_TOKENS);
    run_agent_with_config(task, runner, provider, config).await
}

async fn run_agent_with_config(
    task: &TaskSpec,
    _runner: RunnerKind,
    provider: Arc<dyn LlmProvider>,
    mut config: AppConfig,
) -> Result<RunnerOutput> {
    let root = materialize_workspace(task)?;
    config.workspace_root = root.clone();
    let workspace_note = format!(
        "\n\nWorkspace root for this task: {}",
        root.to_string_lossy()
    );
    config.instructions.push_str(&workspace_note);
    let agent = Agent::new(config, provider);
    let mut rx = agent.start_turn(task.prompt.clone(), CancellationToken::new());
    let mut final_answer = String::new();
    let mut metrics = HarnessMetrics::default();
    let mut trace = Vec::new();

    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Started { .. } => {
                trace.push(TraceEvent {
                    kind: TraceEventKind::Started,
                    text: None,
                    response_id: None,
                    input_tokens: None,
                    output_tokens: None,
                    cached_input_tokens: None,
                });
            }
            AgentEvent::AssistantDelta { delta, .. } => {
                final_answer.push_str(&delta);
                trace.push(TraceEvent {
                    kind: TraceEventKind::TextDelta,
                    text: Some(delta),
                    response_id: None,
                    input_tokens: None,
                    output_tokens: None,
                    cached_input_tokens: None,
                });
            }
            AgentEvent::Completed {
                message,
                response_id,
                cost,
                metrics: turn_metrics,
                ..
            } => {
                if final_answer.is_empty() {
                    final_answer = message.content;
                }
                metrics.tool_calls = turn_metrics.tool_calls;
                metrics.tool_successes = turn_metrics.tool_successes;
                metrics.tool_errors = turn_metrics.tool_errors;
                metrics.tool_denials = turn_metrics.tool_denials;
                metrics.tool_cancellations = turn_metrics.tool_cancellations;
                metrics.files_scanned = turn_metrics.files_scanned;
                metrics.bytes_read = turn_metrics.bytes_read;
                metrics.matches_returned = turn_metrics.matches_returned;
                metrics.receipt_stub_hits = turn_metrics.receipt_stub_hits;
                metrics.negative_receipt_hits = turn_metrics.negative_receipt_hits;
                metrics.spill_writes = turn_metrics.spill_writes;
                metrics.spill_reads = turn_metrics.spill_reads;
                metrics.budget_denials = turn_metrics.budget_denials;
                metrics.redactions = turn_metrics.redactions;
                metrics.input_tokens = cost.input_tokens;
                metrics.output_tokens = cost.output_tokens;
                metrics.cached_input_tokens = cost.cached_input_tokens;
                metrics.estimated_usd_micros = cost.estimated_usd_micros;
                trace.push(trace_completed(response_id, cost));
                break;
            }
            AgentEvent::Failed { error, .. } => {
                let _ = fs::remove_dir_all(&root);
                return Err(error);
            }
            AgentEvent::Cancelled { .. } => {
                let _ = fs::remove_dir_all(&root);
                return Err(SqueezyError::Agent("task was cancelled".to_string()));
            }
            AgentEvent::UserMessage { .. } => {}
            AgentEvent::ToolCallQueued { .. } | AgentEvent::ToolCallCompleted { .. } => {}
            AgentEvent::ToolCallStarted { .. } | AgentEvent::ApprovalRequested { .. } => {}
        }
    }
    let _ = fs::remove_dir_all(&root);
    metrics.output_bytes = final_answer.len() as u64;

    Ok(RunnerOutput {
        final_answer,
        metrics,
        trace,
    })
}

fn trace_completed(response_id: Option<String>, cost: CostSnapshot) -> TraceEvent {
    TraceEvent {
        kind: TraceEventKind::Completed,
        text: None,
        response_id,
        input_tokens: cost.input_tokens,
        output_tokens: cost.output_tokens,
        cached_input_tokens: cost.cached_input_tokens,
    }
}

#[derive(Debug)]
struct ScriptedProvider {
    name: &'static str,
    events: Mutex<VecDeque<TraceEvent>>,
}

impl ScriptedProvider {
    fn new(runner_name: &str, events: Vec<TraceEvent>) -> Self {
        let name = if runner_name.contains("anthropic") {
            "mock-anthropic"
        } else {
            "mock-openai"
        };
        Self {
            name,
            events: Mutex::new(events.into()),
        }
    }
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn stream_response(&self, _request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        let events = self
            .events
            .lock()
            .expect("scripted events")
            .drain(..)
            .map(trace_to_llm_event)
            .collect::<Vec<_>>();
        let stream: Pin<Box<dyn Stream<Item = Result<LlmEvent>> + Send>> =
            Box::pin(stream::iter(events));
        stream
    }
}

fn trace_to_llm_event(event: TraceEvent) -> Result<LlmEvent> {
    match event.kind {
        TraceEventKind::Started => Ok(LlmEvent::Started),
        TraceEventKind::TextDelta => Ok(LlmEvent::TextDelta(event.text.unwrap_or_default())),
        TraceEventKind::Completed => Ok(LlmEvent::Completed {
            response_id: event.response_id,
            cost: CostSnapshot {
                input_tokens: event.input_tokens,
                output_tokens: event.output_tokens,
                cached_input_tokens: event.cached_input_tokens,
                cache_write_input_tokens: None,
                estimated_usd_micros: None,
            },
        }),
    }
}

fn provider_from_config(config: &AppConfig) -> Result<Arc<dyn LlmProvider>> {
    llm_provider_from_config(&config.provider)
}

fn mock_events(task: &TaskSpec, provider: &str) -> Result<Vec<TraceEvent>> {
    let mock = task
        .mock
        .as_ref()
        .ok_or_else(|| SqueezyError::Agent(format!("task {} has no mock spec", task.id)))?;
    let provider_mock = match provider {
        "openai" => mock.openai.as_ref(),
        "anthropic" => mock.anthropic.as_ref(),
        _ => None,
    }
    .ok_or_else(|| SqueezyError::Agent(format!("task {} has no {provider} mock", task.id)))?;
    Ok(provider_mock.events.clone())
}

fn evaluate(task: &TaskSpec, final_answer: &str) -> Correctness {
    let missing = task
        .expect
        .contains
        .iter()
        .filter(|expected| !final_answer.contains(expected.as_str()))
        .map(|expected| format!("missing expected substring: {expected}"))
        .collect::<Vec<_>>();

    Correctness {
        passed: missing.is_empty(),
        reasons: if missing.is_empty() {
            vec!["all expected substrings were present".to_string()]
        } else {
            missing
        },
    }
}

fn validate_task(task: &TaskSpec) -> Result<()> {
    if task.id.trim().is_empty() {
        return Err(SqueezyError::Agent("task id must not be empty".to_string()));
    }
    for file in &task.workspace.files {
        safe_relative_path(&file.path)?;
    }
    if task.expect.contains.is_empty() {
        return Err(SqueezyError::Agent(format!(
            "task {} must declare expect.contains",
            task.id
        )));
    }
    Ok(())
}

fn materialize_workspace(task: &TaskSpec) -> Result<PathBuf> {
    let root = std::env::temp_dir().join(format!(
        "squeezy-harness-{}-{}",
        sanitize(&task.id),
        unique_suffix()
    ));
    fs::create_dir_all(&root)?;
    for file in &task.workspace.files {
        let relative = safe_relative_path(&file.path)?;
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, &file.content)?;
    }
    Ok(root)
}

fn safe_relative_path(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(SqueezyError::Agent(format!(
            "task path must be relative and stay in workspace: {}",
            path.display()
        )));
    }
    Ok(path.to_path_buf())
}

fn list_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    list_files_inner(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn list_files_inner(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type()?.is_dir() {
            if matches!(
                name.as_ref(),
                ".git" | "target" | "vendor" | "node_modules" | "generated"
            ) {
                continue;
            }
            list_files_inner(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

fn path_matches(pattern: &str, path: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        return path.ends_with(&format!(".{suffix}"));
    }
    let normalized = pattern.trim_start_matches('/');
    if path == normalized {
        return true;
    }
    // Literal patterns must match on a path-segment boundary so that
    // `lib.rs` does not also match `src/sublib.rs`.
    path.strip_suffix(normalized)
        .is_some_and(|prefix| prefix.ends_with('/'))
}

fn write_jsonl(path: &Path, results: &[TaskResult]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    for result in results {
        serde_json::to_writer(&mut writer, result)
            .map_err(|err| SqueezyError::Agent(format!("failed to write JSONL: {err}")))?;
        writer.write_all(b"\n")?;
    }
    // Flush explicitly so a failure surfaces here instead of being swallowed
    // by `BufWriter::drop`; the harness JSONL is uploaded as a CI artifact
    // and must not silently truncate.
    writer
        .flush()
        .map_err(|err| SqueezyError::Agent(format!("failed to flush JSONL: {err}")))?;
    Ok(())
}

fn write_trace(
    trace_dir: &Path,
    task: &TaskSpec,
    runner: RunnerKind,
    events: &[TraceEvent],
) -> Result<()> {
    fs::create_dir_all(trace_dir)?;
    let record = TraceRecord {
        task_id: task.id.clone(),
        runner,
        events: events.to_vec(),
    };
    let path = trace_dir.join(format!("{}-{}.json", sanitize(&task.id), runner.name()));
    let json = serde_json::to_string_pretty(&record)
        .map_err(|err| SqueezyError::Agent(format!("failed to serialize trace: {err}")))?;
    fs::write(path, json)?;
    Ok(())
}

fn require_costly(provider: &str) -> Result<()> {
    if std::env::var(COSTLY_FLAG).as_deref() != Ok("1") {
        return Err(SqueezyError::ProviderNotConfigured(format!(
            "{provider} costly runner requires {COSTLY_FLAG}=1"
        )));
    }
    let key = match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "google" => "GEMINI_API_KEY",
        "azure_openai" => "AZURE_OPENAI_API_KEY",
        "ollama" => return Ok(()),
        "bedrock" => {
            if std::env::var("AWS_ACCESS_KEY_ID").is_ok()
                || std::env::var("AWS_PROFILE").is_ok()
                || std::env::var("AWS_WEB_IDENTITY_TOKEN_FILE").is_ok()
            {
                return Ok(());
            }
            return Err(SqueezyError::ProviderNotConfigured(
                "bedrock costly runner requires AWS credentials".to_string(),
            ));
        }
        _ => "OPENAI_API_KEY",
    };
    if std::env::var(key).is_ok_and(|value| !value.trim().is_empty()) {
        Ok(())
    } else {
        Err(SqueezyError::ProviderNotConfigured(format!(
            "{provider} costly runner requires {key}"
        )))
    }
}

fn costly_model(provider: &str) -> String {
    match provider {
        "anthropic" => std::env::var("SQUEEZY_COSTLY_ANTHROPIC_MODEL")
            .or_else(|_| std::env::var("SQUEEZY_COSTLY_MODEL"))
            .unwrap_or_else(|_| DEFAULT_ANTHROPIC_MODEL.to_string()),
        "google" => std::env::var("SQUEEZY_COSTLY_GOOGLE_MODEL")
            .or_else(|_| std::env::var("SQUEEZY_COSTLY_MODEL"))
            .unwrap_or_else(|_| DEFAULT_GOOGLE_MODEL.to_string()),
        "azure_openai" => std::env::var("SQUEEZY_COSTLY_AZURE_OPENAI_MODEL")
            .or_else(|_| std::env::var("SQUEEZY_COSTLY_MODEL"))
            .unwrap_or_else(|_| DEFAULT_AZURE_OPENAI_MODEL.to_string()),
        "ollama" => std::env::var("SQUEEZY_COSTLY_OLLAMA_MODEL")
            .or_else(|_| std::env::var("SQUEEZY_COSTLY_MODEL"))
            .unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string()),
        "bedrock" => std::env::var("SQUEEZY_COSTLY_BEDROCK_MODEL")
            .or_else(|_| std::env::var("SQUEEZY_COSTLY_MODEL"))
            .unwrap_or_else(|_| DEFAULT_BEDROCK_MODEL.to_string()),
        _ => std::env::var("SQUEEZY_COSTLY_OPENAI_MODEL")
            .or_else(|_| std::env::var("SQUEEZY_COSTLY_MODEL"))
            .unwrap_or_else(|_| DEFAULT_OPENAI_MODEL.to_string()),
    }
}

fn costly_max_output_tokens() -> Result<u32> {
    let Ok(raw) = std::env::var("SQUEEZY_COSTLY_MAX_OUTPUT_TOKENS") else {
        return Ok(DEFAULT_MAX_OUTPUT_TOKENS);
    };
    let parsed = raw.parse::<u32>().map_err(|_| {
        SqueezyError::ProviderNotConfigured(
            "SQUEEZY_COSTLY_MAX_OUTPUT_TOKENS must be a positive integer".to_string(),
        )
    })?;
    if parsed == 0 {
        return Err(SqueezyError::ProviderNotConfigured(
            "SQUEEZY_COSTLY_MAX_OUTPUT_TOKENS must be greater than 0".to_string(),
        ));
    }
    Ok(parsed)
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

pub fn summarize(results: &[TaskResult]) -> serde_json::Value {
    let passed = results
        .iter()
        .filter(|result| result.status == TaskStatus::Passed)
        .count();
    json!({
        "total": results.len(),
        "passed": passed,
        "failed": results.len().saturating_sub(passed),
    })
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
