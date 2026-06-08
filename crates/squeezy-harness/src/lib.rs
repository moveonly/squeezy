use std::{
    collections::{BTreeSet, VecDeque},
    fmt,
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
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
    DEFAULT_OPENAI_MODEL, Result, SessionMode, SqueezyError,
};
use squeezy_llm::{
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall,
    provider_from_config as llm_provider_from_config,
};
use squeezy_store::SessionReplayTape;
use tokio_util::sync::CancellationToken;

const COSTLY_FLAG: &str = "SQUEEZY_RUN_COSTLY_TESTS";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum RunnerKind {
    MockOpenai,
    MockAnthropic,
    GrepBaseline,
    Replay,
    PlannerProbe,
    PlannerProbeNoPlanner,
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
        matches!(
            self,
            Self::MockOpenai
                | Self::MockAnthropic
                | Self::PlannerProbe
                | Self::PlannerProbeNoPlanner
        )
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::MockOpenai => "mock-openai",
            Self::MockAnthropic => "mock-anthropic",
            Self::GrepBaseline => "grep-baseline",
            Self::Replay => "replay",
            Self::PlannerProbe => "planner-probe",
            Self::PlannerProbeNoPlanner => "planner-probe-no-planner",
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
        RunnerKind::PlannerProbe,
        RunnerKind::PlannerProbeNoPlanner,
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
    pub replay: Option<ReplaySpec>,
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
pub struct ReplaySpec {
    pub trace: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub mode: Option<SessionMode>,
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
    pub planner_turns: u64,
    pub planner_tool_calls: u64,
    pub planner_refusals: u64,
    pub subagent_calls: u64,
    pub subagent_failures: u64,
    pub subagent_tool_calls: u64,
    pub subagent_budget_denials: u64,
    pub subagent_files_scanned: u64,
    pub subagent_bytes_read: u64,
    pub subagent_model_output_bytes: u64,
    pub redactions: u64,
    pub prompt_bytes: u64,
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

    let mut tasks = Vec::with_capacity(paths.len());
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
    let mut results = Vec::with_capacity(tasks.len() * runners.len());

    for task in &tasks {
        for runner in &runners {
            let result = run_task_with_base(
                task,
                *runner,
                config.trace_dir.as_deref(),
                &config.tasks_dir,
            )
            .await;
            results.push(result);
        }
    }

    if let Some(path) = &config.jsonl_path {
        write_jsonl(path, &results)?;
    }

    Ok(results)
}

pub async fn run_task(task: &TaskSpec, runner: RunnerKind, trace_dir: Option<&Path>) -> TaskResult {
    run_task_with_base(task, runner, trace_dir, Path::new(".")).await
}

async fn run_task_with_base(
    task: &TaskSpec,
    runner: RunnerKind,
    trace_dir: Option<&Path>,
    tasks_dir: &Path,
) -> TaskResult {
    let started = Instant::now();
    let outcome = match runner {
        RunnerKind::MockOpenai => run_mock(task, runner, mock_events(task, "openai")).await,
        RunnerKind::MockAnthropic => run_mock(task, runner, mock_events(task, "anthropic")).await,
        RunnerKind::GrepBaseline => run_baseline(task),
        RunnerKind::Replay => run_replay(task, tasks_dir).await,
        RunnerKind::PlannerProbe => run_planner_probe(task, runner, true).await,
        RunnerKind::PlannerProbeNoPlanner => run_planner_probe(task, runner, false).await,
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
    let mut output = run_agent(task, runner, provider.clone()).await?;
    output.metrics.prompt_bytes = provider.prompt_bytes();
    Ok(output)
}

async fn run_replay(task: &TaskSpec, tasks_dir: &Path) -> Result<RunnerOutput> {
    let replay = task
        .replay
        .as_ref()
        .ok_or_else(|| SqueezyError::Agent(format!("task {} has no replay spec", task.id)))?;
    let path = resolve_harness_path(tasks_dir, &replay.trace);
    let text = fs::read_to_string(&path)?;
    let tape = serde_json::from_str::<SessionReplayTape>(&text).map_err(|err| {
        SqueezyError::Agent(format!(
            "failed to parse replay trace {}: {err}",
            path.display()
        ))
    })?;
    let mut config = AppConfig::from_env();
    disable_product_telemetry(&mut config);
    config.max_output_tokens = DEFAULT_MAX_OUTPUT_TOKENS;
    let provider = replay.provider.as_deref().unwrap_or("mock-openai");
    let model = replay.model.clone().unwrap_or_else(|| provider.to_string());
    let mode = replay.mode.unwrap_or(SessionMode::Build);
    let report = Agent::replay_tape(config, task.id.clone(), tape, provider, model, mode).await?;
    let metrics = HarnessMetrics {
        output_bytes: report.final_answer.len() as u64,
        ..HarnessMetrics::default()
    };
    Ok(RunnerOutput {
        final_answer: report.final_answer,
        metrics,
        trace: Vec::new(),
    })
}

async fn run_planner_probe(
    task: &TaskSpec,
    runner: RunnerKind,
    exploration_graph: bool,
) -> Result<RunnerOutput> {
    let baseline = task.baseline.as_ref().ok_or_else(|| {
        SqueezyError::Agent(format!("task {} has no planner-probe baseline", task.id))
    })?;
    let provider = Arc::new(PlannerProbeProvider::new(task, baseline));
    let mut config = AppConfig::from_env();
    disable_product_telemetry(&mut config);
    config.model = runner.name().to_string();
    config.max_output_tokens = DEFAULT_MAX_OUTPUT_TOKENS;
    config.exploration_graph = exploration_graph;
    run_agent_with_config(task, runner, provider, config).await
}

async fn run_costly(
    task: &TaskSpec,
    runner: RunnerKind,
    provider_name: &str,
    trace_dir: Option<&Path>,
) -> Result<RunnerOutput> {
    require_costly(provider_name)?;
    let mut config = AppConfig::from_env_with_provider(provider_name);
    disable_product_telemetry(&mut config);
    config.model = costly_model(provider_name);
    config.max_output_tokens = costly_max_output_tokens()?;

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
    disable_product_telemetry(&mut config);
    config.model = runner.name().to_string();
    config.max_output_tokens = DEFAULT_MAX_OUTPUT_TOKENS;
    run_agent_with_config(task, runner, provider, config).await
}

async fn run_agent_with_config(
    task: &TaskSpec,
    _runner: RunnerKind,
    provider: Arc<dyn LlmProvider>,
    mut config: AppConfig,
) -> Result<RunnerOutput> {
    let root = materialize_workspace(task)?;
    disable_product_telemetry(&mut config);
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
                metrics.planner_turns = turn_metrics.planner_turns;
                metrics.planner_tool_calls = turn_metrics.planner_tool_calls;
                metrics.planner_refusals = turn_metrics.planner_refusals;
                metrics.subagent_calls = turn_metrics.subagent_calls;
                metrics.subagent_failures = turn_metrics.subagent_failures;
                metrics.subagent_tool_calls = turn_metrics.subagent_tool_calls;
                metrics.subagent_budget_denials = turn_metrics.subagent_budget_denials;
                metrics.subagent_files_scanned = turn_metrics.subagent_files_scanned;
                metrics.subagent_bytes_read = turn_metrics.subagent_bytes_read;
                metrics.subagent_model_output_bytes = turn_metrics.subagent_model_output_bytes;
                metrics.redactions = turn_metrics.redactions;
                metrics.input_tokens = cost.input_tokens;
                metrics.output_tokens = cost.output_tokens;
                metrics.cached_input_tokens = cost.cached_input_tokens;
                metrics.estimated_usd_micros = cost.estimated_usd_micros;
                trace.push(trace_completed(response_id, cost));
                break;
            }
            AgentEvent::Failed { error, .. } => {
                // Shut the agent down before deleting its workspace so the
                // background event-loop drops every redb handle it owns.
                // Without this, the subsequent `remove_dir_all` races the
                // agent's exclusive lock on `state.redb` / `graph.redb` on
                // Windows. See `Agent::shutdown` in `squeezy-agent`.
                agent.shutdown().await;
                let _ = fs::remove_dir_all(&root);
                return Err(error);
            }
            AgentEvent::Cancelled { .. } => {
                // Same rationale as `Failed` above: shutdown before
                // workspace teardown to release the Windows redb locks.
                agent.shutdown().await;
                let _ = fs::remove_dir_all(&root);
                return Err(SqueezyError::Agent("task was cancelled".to_string()));
            }
            AgentEvent::UserMessage { .. } => {}
            AgentEvent::ToolCallQueued { .. } | AgentEvent::ToolCallCompleted { .. } => {}
            AgentEvent::ToolCallStarted { .. }
            | AgentEvent::ApprovalRequested { .. }
            | AgentEvent::SkillActivationWarning { .. }
            | AgentEvent::TaskStateUpdated { .. }
            | AgentEvent::McpStatusUpdated { .. }
            | AgentEvent::McpElicitationRequested { .. }
            | AgentEvent::RequestUserInputRequested { .. }
            | AgentEvent::ContextCompacted { .. }
            | AgentEvent::ContextUsageUpdate { .. }
            | AgentEvent::SubagentStarted { .. }
            | AgentEvent::SubagentActivity { .. }
            | AgentEvent::SubagentToolResult { .. }
            | AgentEvent::SubagentCompleted { .. }
            | AgentEvent::SubagentFailed { .. }
            | AgentEvent::SubagentRejected { .. }
            | AgentEvent::AiReviewerTripped { .. } => {}
            AgentEvent::JobUpdated { .. } | AgentEvent::JobNotification { .. } => {}
            AgentEvent::CostWarning { .. }
            | AgentEvent::CostCapUnenforceable { .. }
            | AgentEvent::CostUpdate { .. }
            | AgentEvent::ToolProgress { .. }
            | AgentEvent::ReasoningDelta { .. }
            | AgentEvent::ReasoningSegment { .. }
            | AgentEvent::ShellSandboxBestEffortFallback { .. }
            | AgentEvent::TurnRouted { .. } => {}
        }
    }
    // Final success path: shut down the agent before removing the workspace
    // so its redb handles release the Windows exclusive lock. See the
    // `Agent::shutdown` docstring in `squeezy-agent`.
    agent.shutdown().await;
    let _ = fs::remove_dir_all(&root);
    metrics.output_bytes = final_answer.len() as u64;

    Ok(RunnerOutput {
        final_answer,
        metrics,
        trace,
    })
}

fn disable_product_telemetry(config: &mut AppConfig) {
    config.telemetry.enabled = false;
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
    // `prompt_bytes` is updated exactly once per request and read at most
    // once per task, so contention is effectively zero. Using `AtomicU64`
    // avoids the awkward poisoning-on-panic surface that `Mutex<u64>` would
    // introduce while keeping the increment lock-free.
    prompt_bytes: AtomicU64,
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
            prompt_bytes: AtomicU64::new(0),
        }
    }

    fn prompt_bytes(&self) -> u64 {
        self.prompt_bytes.load(Ordering::Relaxed)
    }
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn stream_response(&self, request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        self.prompt_bytes
            .fetch_add(request_prompt_bytes(&request), Ordering::Relaxed);
        let events = {
            let mut guard = self.events.lock().expect("scripted events");
            std::mem::take(&mut *guard)
        };
        let stream: Pin<Box<dyn Stream<Item = Result<LlmEvent>> + Send>> =
            Box::pin(stream::iter(events.into_iter().map(trace_to_llm_event)));
        stream
    }
}

fn request_prompt_bytes(request: &LlmRequest) -> u64 {
    let input_bytes = request
        .input
        .iter()
        .map(|item| format!("{item:?}").len() as u64)
        .sum::<u64>();
    let tool_bytes = request
        .tools
        .iter()
        .map(|tool| {
            (tool.name.len()
                + tool.description.len()
                + tool.parameters.to_string().len()
                + usize::from(tool.strict)) as u64
        })
        .sum::<u64>();
    request.instructions.len() as u64 + input_bytes + tool_bytes
}

#[derive(Debug)]
struct PlannerProbeProvider {
    answer: String,
    pattern: String,
    include: Vec<String>,
    read_path: Option<String>,
    phase: Mutex<u8>,
}

impl PlannerProbeProvider {
    fn new(task: &TaskSpec, baseline: &BaselineSpec) -> Self {
        Self {
            answer: task.expect.contains.join(" "),
            pattern: baseline.pattern.clone(),
            include: baseline.include.clone(),
            read_path: baseline
                .read_path
                .clone()
                .or_else(|| first_expected_source_path(task)),
            phase: Mutex::new(0),
        }
    }
}

impl LlmProvider for PlannerProbeProvider {
    fn name(&self) -> &'static str {
        "planner-probe"
    }

    fn stream_response(&self, request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        let has_planner_context = request.input.iter().any(|item| match item {
            LlmInputItem::FunctionCall { call_id, .. }
            | LlmInputItem::FunctionCallOutput { call_id, .. } => call_id.starts_with("planner_"),
            _ => false,
        });
        let has_read_output = request.input.iter().any(|item| match item {
            LlmInputItem::FunctionCallOutput { call_id, .. } => call_id == "probe_read",
            _ => false,
        });
        let has_grep_output = request.input.iter().any(|item| match item {
            LlmInputItem::FunctionCallOutput { call_id, .. } => call_id == "probe_grep",
            _ => false,
        });

        let mut phase = self.phase.lock().expect("planner probe phase");
        let events = if has_planner_context
            || has_read_output
            || (has_grep_output && self.read_path.is_none())
        {
            vec![
                Ok(LlmEvent::TextDelta(self.answer.clone())),
                Ok(LlmEvent::Completed {
                    response_id: Some("planner_probe_final".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: None,
                    reasoning_only_stop: false,
                }),
            ]
        } else if *phase == 0 {
            *phase = 1;
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::ToolCall(LlmToolCall {
                    call_id: "probe_grep".to_string(),
                    name: "grep".to_string(),
                    arguments: json!({
                        "pattern": self.pattern.clone(),
                        "include": self.include.clone(),
                        "output_mode": "content",
                    }),
                })),
                Ok(LlmEvent::Completed {
                    response_id: Some("planner_probe_grep".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: None,
                    reasoning_only_stop: false,
                }),
            ]
        } else {
            *phase = 2;
            let path = self.read_path.clone().unwrap_or_else(|| ".".to_string());
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::ToolCall(LlmToolCall {
                    call_id: "probe_read".to_string(),
                    name: "read_file".to_string(),
                    arguments: json!({ "path": path }),
                })),
                Ok(LlmEvent::Completed {
                    response_id: Some("planner_probe_read".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: None,
                    reasoning_only_stop: false,
                }),
            ]
        };
        Box::pin(stream::iter(events))
    }
}

fn first_expected_source_path(task: &TaskSpec) -> Option<String> {
    task.expect.contains.iter().find_map(|expected| {
        let value = expected.trim();
        if looks_like_source_path(value) {
            Some(value.trim_end_matches(':').to_string())
        } else {
            None
        }
    })
}

/// Returns true when `value` looks like a path to a source file the
/// planner-probe provider should ask `read_file` for. Matching is anchored
/// at the trailing path segment so strings like `docs/typescript.md` or
/// `copy.py-file` are not mistaken for `.ts` / `.py` paths.
fn looks_like_source_path(value: &str) -> bool {
    const SOURCE_EXTENSIONS: &[&str] = &["rs", "py", "js", "ts"];
    let path_segment = match value.rsplit_once(['/', '\\']) {
        Some((_, tail)) => tail,
        None => value,
    };
    // Strip an optional `:line[:col]` suffix that some `expect.contains`
    // entries use to point at a specific location (e.g. `src/lib.rs:42`).
    let without_loc = path_segment.split(':').next().unwrap_or(path_segment);
    let extension = match without_loc.rsplit_once('.') {
        Some((_, ext)) => ext,
        None => return false,
    };
    SOURCE_EXTENSIONS.contains(&extension)
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
                reasoning_output_tokens: None,
                cached_input_tokens: event.cached_input_tokens,
                cache_write_input_tokens: None,
                estimated_usd_micros: None,
            },
            stop_reason: None,
            reasoning_only_stop: false,
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

fn resolve_harness_path(base: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
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
        return path
            .strip_suffix(suffix)
            .is_some_and(|prefix| prefix.ends_with('.'));
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

fn costly_max_output_tokens() -> Result<Option<u32>> {
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
    Ok(Some(parsed))
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
    let tool_calls = results
        .iter()
        .map(|result| result.metrics.tool_calls)
        .sum::<u64>();
    let bytes_read = results
        .iter()
        .map(|result| result.metrics.bytes_read)
        .sum::<u64>();
    let planner_turns = results
        .iter()
        .map(|result| result.metrics.planner_turns)
        .sum::<u64>();
    let planner_tool_calls = results
        .iter()
        .map(|result| result.metrics.planner_tool_calls)
        .sum::<u64>();
    let planner_refusals = results
        .iter()
        .map(|result| result.metrics.planner_refusals)
        .sum::<u64>();
    json!({
        "total": results.len(),
        "passed": passed,
        "failed": results.len().saturating_sub(passed),
        "tool_calls": tool_calls,
        "bytes_read": bytes_read,
        "planner_turns": planner_turns,
        "planner_tool_calls": planner_tool_calls,
        "planner_refusals": planner_refusals,
    })
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
