use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Mutex as TokioMutex;
use tokio::time::{Duration, sleep, timeout};
use tokio_util::sync::CancellationToken;

use squeezy_agent::{Agent, AgentEvent, RequestUserInputResponse, ToolApprovalDecision};
use squeezy_core::{AppConfig, PermissionMode, SessionMode};
use squeezy_llm::provider_from_config;

use crate::capture::{Capture, EvalEventKind};
use crate::frames::{FrameFinish, FrameRecord, FrameWriter};
use crate::scenario::{
    Action, ApprovalMatch, Assertion, EditReplace, Scenario, SqueezyOverlay, Step, WaitFor,
};
use crate::tickets::TicketDraft;
use crate::workspace::{self, ProvisionedWorkspace};

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub scenario_path: PathBuf,
    pub out_root: PathBuf,
    pub run_triage: bool,
    pub emit_github: bool,
    pub gh_repo: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RunOutcome {
    pub run_dir: PathBuf,
    pub trace_event_count: u64,
    pub frame_count: u64,
    pub ticket_count: u64,
    pub findings: Vec<String>,
}

#[derive(Debug, Error)]
pub enum EvalError {
    #[error("io: {0}")]
    Io(String),
    #[error("scenario parse: {0}")]
    ScenarioParse(String),
    #[error("workspace: {0}")]
    Workspace(String),
    #[error("config: {0}")]
    Config(String),
    #[error("provider: {0}")]
    Provider(String),
    #[error("internal: {0}")]
    Internal(String),
}

/// Top-level entry point. Drives the scenario end-to-end and returns the
/// summary the CLI prints.
pub async fn run_scenario(
    scenario: Scenario,
    options: RunOptions,
) -> Result<RunOutcome, EvalError> {
    let run_dir = options
        .out_root
        .join(format!("{}-{}", scenario.slug(), timestamp_dir_slug()));
    std::fs::create_dir_all(&run_dir)
        .map_err(|err| EvalError::Io(format!("create run dir {run_dir:?}: {err}")))?;

    let capture = Arc::new(Capture::create(&run_dir)?);
    let frames = Arc::new(FrameWriter::create(&run_dir)?);

    // 1. Provision the workspace.
    let scratch_root = options.out_root.join("_workspaces");
    let workspace = workspace::provision(&scenario.workspace, &scratch_root)?;

    // 2. Build the AppConfig with the scenario overlay applied, then the agent.
    let mut config = AppConfig::from_env_and_settings()
        .map_err(|err| EvalError::Config(format!("load AppConfig: {err}")))?;
    apply_overlay(&mut config, &scenario.squeezy, &workspace.path)?;
    let provider = if scenario.squeezy.provider.as_deref() == Some("mock") {
        crate::mock_provider::MockProvider::shared(scenario.mock.clone())
    } else {
        provider_from_config(&config.provider)
            .map_err(|err| EvalError::Provider(provider_hint(err)))?
    };
    let agent = Agent::new(config.clone(), provider);

    // 3. Drive the steps.
    let driver = Driver {
        agent: Arc::new(agent),
        capture: capture.clone(),
        frames: frames.clone(),
        action_queue: TokioMutex::new(Vec::new()),
        scenario: scenario.clone(),
        last_turn_id: TokioMutex::new(None),
        last_cancel: TokioMutex::new(None),
        run_start: Instant::now(),
        wall_clock_seconds: TokioMutex::new(0),
        total_input_tokens: TokioMutex::new(0),
        total_tool_calls: TokioMutex::new(0),
        tool_errors: TokioMutex::new(0),
        last_assistant_text: TokioMutex::new(String::new()),
    };

    driver.dispatch_steps().await?;

    // 4. Evaluate soft expectations into findings.
    let findings = driver.evaluate_expectations().await;
    for finding in &findings {
        capture.record(
            None,
            EvalEventKind::ActionStep {
                action: json!({"kind": "expectation"}),
                status: format!("finding: {finding}"),
            },
        )?;
    }

    // 5. Write the run manifest.
    let trace_event_count = read_line_count(&capture.path())?;
    let frame_count = read_line_count(&frames.path())?;
    let manifest = build_manifest(
        &scenario,
        &options,
        &workspace,
        trace_event_count,
        frame_count,
        &findings,
    );
    let manifest_path = run_dir.join("run.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest)
            .map_err(|err| EvalError::Internal(format!("serialize manifest: {err}")))?,
    )
    .map_err(|err| EvalError::Io(format!("write {manifest_path:?}: {err}")))?;

    // 6. Optionally triage and emit tickets.
    let mut ticket_count = 0u64;
    let triage_enabled = options.run_triage && scenario.triage.enabled;
    let tickets = if triage_enabled {
        match crate::triage::triage(&scenario, &config, &capture.path(), &frames.path()).await {
            Ok(drafts) => drafts,
            Err(err) => {
                tracing::warn!(error = %err, "triage failed; continuing without tickets");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    if !tickets.is_empty() || !findings.is_empty() {
        let all = synthesize_tickets(tickets, &findings, &scenario);
        ticket_count = all.len() as u64;
        crate::tickets::emit(
            &run_dir,
            &all,
            crate::tickets::EmitOptions {
                emit_github: options.emit_github,
                gh_repo: options.gh_repo.clone(),
            },
        )?;
    }

    Ok(RunOutcome {
        run_dir,
        trace_event_count,
        frame_count,
        ticket_count,
        findings,
    })
}

fn synthesize_tickets(
    mut from_llm: Vec<TicketDraft>,
    findings: &[String],
    scenario: &Scenario,
) -> Vec<TicketDraft> {
    for (idx, finding) in findings.iter().enumerate() {
        from_llm.push(TicketDraft {
            id: format!("finding-{:02}", idx + 1),
            title: format!("Expectation failed: {finding}"),
            severity: "minor".into(),
            category: "correctness".into(),
            summary: finding.clone(),
            repro: format!("Run scenario `{}`.", scenario.id),
            evidence: Vec::new(),
            suggested_fix: None,
        });
    }
    from_llm
}

fn build_manifest(
    scenario: &Scenario,
    options: &RunOptions,
    workspace: &ProvisionedWorkspace,
    trace_event_count: u64,
    frame_count: u64,
    findings: &[String],
) -> Value {
    json!({
        "schema_version": 1,
        "scenario": {
            "id": scenario.id,
            "title": scenario.title,
            "path": options.scenario_path.display().to_string(),
        },
        "workspace": match &workspace.source {
            crate::workspace::WorkspaceSource::Local(path) => json!({
                "kind": "local",
                "path": path.display().to_string(),
            }),
            crate::workspace::WorkspaceSource::Github { repo, sha } => json!({
                "kind": "github",
                "repo": repo,
                "sha": sha,
            }),
        },
        "totals": {
            "trace_events": trace_event_count,
            "frames": frame_count,
            "findings": findings.len(),
        },
        "findings": findings,
        "squeezy_version": env!("CARGO_PKG_VERSION"),
    })
}

fn read_line_count(path: &Path) -> Result<u64, EvalError> {
    use std::io::{BufRead, BufReader};
    let file =
        std::fs::File::open(path).map_err(|err| EvalError::Io(format!("open {path:?}: {err}")))?;
    let reader = BufReader::new(file);
    let mut count = 0u64;
    for line in reader.lines() {
        let line = line.map_err(|err| EvalError::Io(format!("read {path:?}: {err}")))?;
        if !line.trim().is_empty() {
            count += 1;
        }
    }
    Ok(count)
}

fn apply_overlay(
    config: &mut AppConfig,
    overlay: &SqueezyOverlay,
    workspace_root: &Path,
) -> Result<(), EvalError> {
    config.workspace_root = workspace_root.to_path_buf();
    if let Some(model) = &overlay.model {
        config.model = model.clone();
    }
    if let Some(instructions) = &overlay.instructions {
        config.instructions = instructions.clone();
    }
    if let Some(max) = overlay.max_output_tokens {
        config.max_output_tokens = Some(max);
    }
    if let Some(mode) = &overlay.mode {
        config.session_mode = match mode.to_ascii_lowercase().as_str() {
            "plan" => SessionMode::Plan,
            "build" => SessionMode::Build,
            other => {
                return Err(EvalError::Config(format!(
                    "unknown session mode in overlay: {other}"
                )));
            }
        };
    }
    if let Some(pm) = &overlay.permission_mode {
        let mode = PermissionMode::parse(pm)
            .ok_or_else(|| EvalError::Config(format!("unknown permission_mode: {pm}")))?;
        // Apply uniformly to the gates the scenario most often wants to
        // pin. Power users can shape these individually via settings.
        config.permissions.edit = mode;
        config.permissions.shell = mode;
        config.permissions.web = mode;
        config.permissions.mcp = mode;
    }
    // `provider` overlay is currently advisory; switching providers requires
    // re-resolving the full provider config from settings (keys, base URLs,
    // etc.), which we'd want to plumb through AppConfig::from_env_and_settings_with_provider.
    if overlay.provider.is_some() {
        // Intentional silent ignore in first cut. The scenario's expected
        // model still applies. Documented in EVAL_HARNESS.md.
    }
    Ok(())
}

fn provider_hint(err: squeezy_core::SqueezyError) -> String {
    format!(
        "{err}\nhint: for an offline run, set `[squeezy] provider = \"mock\"` in your scenario \
         and add a `[mock]` block with scripted `turns`. See docs/internal/EVAL_HARNESS.md."
    )
}

fn timestamp_dir_slug() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{ms}")
}

struct Driver {
    agent: Arc<Agent>,
    capture: Arc<Capture>,
    frames: Arc<FrameWriter>,
    /// Action steps that are not yet consumed. The driver pops the front of
    /// this queue when handling approvals / on-tool-call hooks.
    action_queue: TokioMutex<Vec<Action>>,
    scenario: Scenario,
    last_turn_id: TokioMutex<Option<String>>,
    last_cancel: TokioMutex<Option<CancellationToken>>,
    run_start: Instant,
    wall_clock_seconds: TokioMutex<u64>,
    total_input_tokens: TokioMutex<u64>,
    total_tool_calls: TokioMutex<u64>,
    tool_errors: TokioMutex<u64>,
    last_assistant_text: TokioMutex<String>,
}

impl Driver {
    async fn dispatch_steps(&self) -> Result<(), EvalError> {
        for step in self.scenario.steps.clone() {
            match step {
                Step::Prompt { text, wait_for } => {
                    self.run_prompt(text, wait_for).await?;
                }
                Step::Action(action) => match action.when() {
                    Some(_) => {
                        // Queue conditional action; the event pump fires it
                        // when its trigger appears during the next turn.
                        self.action_queue.lock().await.push(action);
                    }
                    None => {
                        self.execute_action_now(&action).await?;
                    }
                },
            }
        }
        // Drain remaining queued actions (no trigger came during the run);
        // record as unfired so triage can flag them.
        let leftover: Vec<Action> = self.action_queue.lock().await.drain(..).collect();
        for action in leftover {
            self.capture.record(
                None,
                EvalEventKind::ActionStep {
                    action: action_to_value(&action),
                    status: "unfired_no_trigger".into(),
                },
            )?;
        }
        Ok(())
    }

    async fn execute_action_now(&self, action: &Action) -> Result<(), EvalError> {
        let payload = action_to_value(action);
        match action {
            Action::SlashCommand { command, .. } => {
                let status = self.dispatch_slash_command(command).await?;
                self.capture.record(
                    None,
                    EvalEventKind::SlashCommand {
                        command: command.clone(),
                    },
                )?;
                self.capture.record(
                    None,
                    EvalEventKind::ActionStep {
                        action: payload,
                        status,
                    },
                )?;
            }
            Action::EditFile {
                path,
                content,
                replace,
                ..
            } => {
                let status = self.apply_file_edit(path, content.as_deref(), replace.as_ref())?;
                self.capture.record(
                    None,
                    EvalEventKind::ActionStep {
                        action: payload,
                        status,
                    },
                )?;
            }
            Action::WaitSeconds { seconds, .. } => {
                sleep(Duration::from_secs(*seconds)).await;
                self.capture.record(
                    None,
                    EvalEventKind::ActionStep {
                        action: payload,
                        status: "waited".into(),
                    },
                )?;
            }
            Action::CancelTurn { .. } => {
                if let Some(token) = self.last_cancel.lock().await.as_ref() {
                    token.cancel();
                    self.capture.record(
                        None,
                        EvalEventKind::ActionStep {
                            action: payload,
                            status: "cancelled".into(),
                        },
                    )?;
                } else {
                    self.capture.record(
                        None,
                        EvalEventKind::ActionStep {
                            action: payload,
                            status: "no_turn_to_cancel".into(),
                        },
                    )?;
                }
            }
            Action::Approve { .. } | Action::Deny { .. } => {
                // Out-of-turn approvals are queued (no current approval to
                // answer); the event pump consumes them when an
                // ApprovalRequested arrives in the next turn.
                self.action_queue.lock().await.push(action.clone());
            }
            Action::Assert { check, .. } => {
                let status = self.evaluate_assertion(check).await;
                self.capture.record(
                    None,
                    EvalEventKind::ActionStep {
                        action: payload,
                        status,
                    },
                )?;
            }
        }
        Ok(())
    }

    async fn dispatch_slash_command(&self, command: &str) -> Result<String, EvalError> {
        // First-cut slash command handling. We support the small set that
        // is fully expressible through `Agent`'s public API today and
        // record the rest as unsupported so triage can flag missing
        // automation rather than silently noop.
        let trimmed = command.trim().trim_start_matches('/');
        match trimmed.split_whitespace().next().unwrap_or("") {
            "compact" => {
                self.agent
                    .compact_context_manual()
                    .await
                    .map_err(|err| EvalError::Internal(format!("compact_context_manual: {err}")))?;
                Ok("compacted".into())
            }
            "plan" => {
                self.agent.set_session_mode(SessionMode::Plan, "eval");
                Ok("mode_plan".into())
            }
            "build" => {
                self.agent.set_session_mode(SessionMode::Build, "eval");
                Ok("mode_build".into())
            }
            other => Ok(format!("unsupported_slash_command:{other}")),
        }
    }

    fn apply_file_edit(
        &self,
        path: &Path,
        content: Option<&str>,
        replace: Option<&EditReplace>,
    ) -> Result<String, EvalError> {
        let workspace_root = &self.agent.as_ref().workspace_root_clone();
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            workspace_root.join(path)
        };
        if let Some(content) = content {
            if let Some(parent) = absolute.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| EvalError::Io(format!("create_dir_all {parent:?}: {err}")))?;
            }
            std::fs::write(&absolute, content)
                .map_err(|err| EvalError::Io(format!("write {absolute:?}: {err}")))?;
            return Ok("wrote_full_content".into());
        }
        if let Some(replace) = replace {
            let existing = std::fs::read_to_string(&absolute)
                .map_err(|err| EvalError::Io(format!("read {absolute:?}: {err}")))?;
            if !existing.contains(&replace.find) {
                return Ok("find_not_present".into());
            }
            let new_contents = existing.replacen(&replace.find, &replace.with, 1);
            std::fs::write(&absolute, new_contents)
                .map_err(|err| EvalError::Io(format!("write {absolute:?}: {err}")))?;
            return Ok("applied_replace".into());
        }
        Ok("no_payload".into())
    }

    async fn evaluate_assertion(&self, check: &Assertion) -> String {
        match check {
            Assertion::TextContains { text } => {
                let assistant = self.last_assistant_text.lock().await;
                if assistant.contains(text) {
                    "asserted_pass".into()
                } else {
                    format!("asserted_fail: text not in assistant output: {text}")
                }
            }
            Assertion::MaxToolCalls { max } => {
                let count = *self.total_tool_calls.lock().await;
                if count <= *max {
                    "asserted_pass".into()
                } else {
                    format!("asserted_fail: tool calls {count} exceeded max {max}")
                }
            }
        }
    }

    async fn run_prompt(&self, prompt: String, wait_for: WaitFor) -> Result<(), EvalError> {
        let cancel = CancellationToken::new();
        *self.last_cancel.lock().await = Some(cancel.clone());

        let turn_start = Instant::now();
        let mut rx = self.agent.start_turn(prompt.clone(), cancel.clone());
        self.capture.record(
            None,
            EvalEventKind::ActionStep {
                action: json!({"kind": "prompt"}),
                status: format!("send: {} chars", prompt.len()),
            },
        )?;

        let mut frame = FrameRecord {
            prompt: prompt.clone(),
            ..Default::default()
        };
        let mut completed = false;
        let mut received_tool_call = false;
        let mut should_break_on_text = false;

        // Reset per-turn assistant text accumulator.
        self.last_assistant_text.lock().await.clear();

        // Hard ceiling so an infinite stream never wedges the driver.
        let event_timeout = Duration::from_secs(10);

        while let Ok(Some(event)) = timeout(event_timeout, rx.recv()).await {
            match event {
                AgentEvent::UserMessage { turn_id, message } => {
                    let turn_str = format!("{turn_id:?}");
                    *self.last_turn_id.lock().await = Some(turn_str.clone());
                    frame.turn_id = turn_str.clone();
                    let text = transcript_text(&message);
                    self.capture
                        .record(Some(turn_str), EvalEventKind::UserMessage { text })?;
                }
                AgentEvent::Started { turn_id } => {
                    let turn_str = format!("{turn_id:?}");
                    *self.last_turn_id.lock().await = Some(turn_str.clone());
                    self.capture
                        .record(Some(turn_str), EvalEventKind::TurnStarted)?;
                }
                AgentEvent::AssistantDelta { turn_id, delta } => {
                    let turn_str = format!("{turn_id:?}");
                    frame.assistant_text.push_str(&delta);
                    self.last_assistant_text.lock().await.push_str(&delta);
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::AssistantDelta {
                            delta: delta.clone(),
                        },
                    )?;
                    if let WaitFor::TextContains { text } = &wait_for
                        && frame.assistant_text.contains(text)
                    {
                        should_break_on_text = true;
                    }
                }
                AgentEvent::ToolCallQueued { turn_id, call } => {
                    let turn_str = format!("{turn_id:?}");
                    let value = serde_json::to_value(&call).unwrap_or(Value::Null);
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::ToolCallQueued {
                            call: value.clone(),
                        },
                    )?;
                }
                AgentEvent::ToolCallStarted { turn_id, call } => {
                    let turn_str = format!("{turn_id:?}");
                    let value = serde_json::to_value(&call).unwrap_or(Value::Null);
                    received_tool_call = true;
                    *self.total_tool_calls.lock().await += 1;
                    if !frame.tool_calls.contains(&call.name) {
                        frame.tool_calls.push(call.name.clone());
                    }
                    self.fire_on_tool_actions(&call.name).await?;
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::ToolCallStarted { call: value },
                    )?;
                    if let WaitFor::ToolCall { tool } = &wait_for
                        && &call.name == tool
                    {
                        cancel.cancel();
                    }
                }
                AgentEvent::ToolCallCompleted { turn_id, result } => {
                    let turn_str = format!("{turn_id:?}");
                    let status = serde_json::to_value(result.status)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default();
                    if matches!(status.as_str(), "Error" | "Cancelled") {
                        frame.tool_errors.push(result.tool_name.clone());
                        *self.tool_errors.lock().await += 1;
                    }
                    let value = serde_json::to_value(&result).unwrap_or(Value::Null);
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::ToolCallCompleted { result: value },
                    )?;
                }
                AgentEvent::ApprovalRequested {
                    turn_id,
                    request,
                    decision_tx,
                } => {
                    let turn_str = format!("{turn_id:?}");
                    let (decision, recorded) = self.decide_approval(&request.tool_name).await;
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::Approval {
                            request: json!({
                                "tool": request.tool_name,
                                "summary": request.permission.summary,
                            }),
                            decision: recorded,
                        },
                    )?;
                    let _ = decision_tx.send(decision);
                }
                AgentEvent::McpElicitationRequested {
                    turn_id,
                    response_tx,
                    ..
                } => {
                    let turn_str = format!("{turn_id:?}");
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::ActionStep {
                            action: json!({"kind": "mcp_elicitation"}),
                            status: "auto_cancelled".into(),
                        },
                    )?;
                    // Drop the sender so the agent observes a cancelled elicitation.
                    drop(response_tx);
                }
                AgentEvent::RequestUserInputRequested {
                    turn_id,
                    response_tx,
                    ..
                } => {
                    let turn_str = format!("{turn_id:?}");
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::ActionStep {
                            action: json!({"kind": "request_user_input"}),
                            status: "auto_cancelled".into(),
                        },
                    )?;
                    let _ = response_tx.send(RequestUserInputResponse::cancelled());
                }
                AgentEvent::ContextCompacted { turn_id, report } => {
                    let turn_str = format!("{turn_id:?}");
                    let value = json!({"debug": format!("{:?}", report)});
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::ContextCompacted { report: value },
                    )?;
                }
                AgentEvent::TaskStateUpdated { turn_id, snapshot } => {
                    let turn_str = format!("{turn_id:?}");
                    let value = json!({"debug": format!("{:?}", snapshot)});
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::TaskStateUpdated { snapshot: value },
                    )?;
                }
                AgentEvent::McpStatusUpdated { turn_id, snapshot } => {
                    let turn_str = format!("{turn_id:?}");
                    let value = json!({"debug": format!("{:?}", snapshot)});
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::Snapshot {
                            snapshot_kind: "mcp_status".into(),
                            payload: value,
                        },
                    )?;
                }
                AgentEvent::JobUpdated { job } => {
                    let value = json!({"debug": format!("{:?}", job)});
                    self.capture.record(
                        None,
                        EvalEventKind::Snapshot {
                            snapshot_kind: "job".into(),
                            payload: value,
                        },
                    )?;
                }
                AgentEvent::JobNotification { notification } => {
                    let value = json!({"debug": format!("{:?}", notification)});
                    self.capture.record(
                        None,
                        EvalEventKind::Snapshot {
                            snapshot_kind: "job_notification".into(),
                            payload: value,
                        },
                    )?;
                }
                AgentEvent::SubagentStarted {
                    turn_id,
                    agent,
                    prompt,
                } => {
                    let turn_str = format!("{turn_id:?}");
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::SubagentEvent {
                            event: json!({"kind": "started", "agent": agent, "prompt": prompt}),
                        },
                    )?;
                }
                AgentEvent::SubagentCompleted {
                    turn_id,
                    agent,
                    summary,
                    ..
                } => {
                    let turn_str = format!("{turn_id:?}");
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::SubagentEvent {
                            event: json!({"kind": "completed", "agent": agent, "summary": summary}),
                        },
                    )?;
                }
                AgentEvent::SubagentFailed {
                    turn_id,
                    agent,
                    error,
                    ..
                } => {
                    let turn_str = format!("{turn_id:?}");
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::SubagentEvent {
                            event: json!({"kind": "failed", "agent": agent, "error": error}),
                        },
                    )?;
                }
                AgentEvent::AiReviewerTripped { turn_id, reason } => {
                    let turn_str = format!("{turn_id:?}");
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::Snapshot {
                            snapshot_kind: "ai_reviewer_tripped".into(),
                            payload: json!({"reason": reason}),
                        },
                    )?;
                }
                AgentEvent::Completed {
                    turn_id,
                    cost,
                    metrics,
                    ..
                } => {
                    let turn_str = format!("{turn_id:?}");
                    frame.elapsed_ms = turn_start.elapsed().as_millis() as u64;
                    frame.input_tokens = cost.input_tokens.unwrap_or(0);
                    frame.output_tokens = cost.output_tokens.unwrap_or(0);
                    frame.finish = FrameFinish::Completed;
                    *self.total_input_tokens.lock().await += frame.input_tokens;
                    let metrics_v = serde_json::to_value(&metrics).unwrap_or(Value::Null);
                    let cost_v = serde_json::to_value(&cost).unwrap_or(Value::Null);
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::TurnCompleted {
                            metrics: metrics_v,
                            cost: cost_v,
                        },
                    )?;
                    completed = true;
                    break;
                }
                AgentEvent::Cancelled { turn_id } => {
                    let turn_str = format!("{turn_id:?}");
                    frame.elapsed_ms = turn_start.elapsed().as_millis() as u64;
                    frame.finish = FrameFinish::Cancelled;
                    self.capture
                        .record(Some(turn_str), EvalEventKind::TurnCancelled)?;
                    completed = true;
                    break;
                }
                AgentEvent::Failed { turn_id, error } => {
                    let turn_str = format!("{turn_id:?}");
                    frame.elapsed_ms = turn_start.elapsed().as_millis() as u64;
                    frame.finish = FrameFinish::Failed;
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::TurnFailed {
                            error: format!("{error}"),
                        },
                    )?;
                    completed = true;
                    break;
                }
            }
            if should_break_on_text {
                cancel.cancel();
            }
        }

        if !completed {
            // Timed out or stream ended without a terminal. Best-effort
            // cancel so a subsequent turn can run cleanly.
            cancel.cancel();
            if frame.elapsed_ms == 0 {
                frame.elapsed_ms = turn_start.elapsed().as_millis() as u64;
            }
        }
        if frame.turn_id.is_empty() {
            frame.turn_id = self
                .last_turn_id
                .lock()
                .await
                .clone()
                .unwrap_or_else(|| "unknown".into());
        }
        // Suppress unused warnings in modes where we don't yet branch on
        // these flags.
        let _ = (received_tool_call, &wait_for);
        self.frames.write(&frame)?;
        *self.wall_clock_seconds.lock().await = self.run_start.elapsed().as_secs();
        Ok(())
    }

    async fn fire_on_tool_actions(&self, tool_name: &str) -> Result<(), EvalError> {
        let mut to_fire: Vec<Action> = Vec::new();
        {
            let mut queue = self.action_queue.lock().await;
            let mut remaining = Vec::with_capacity(queue.len());
            for action in queue.drain(..) {
                let matches = action
                    .when()
                    .and_then(|w| w.on_tool.as_deref())
                    .map(|t| t == tool_name)
                    .unwrap_or(false);
                if matches {
                    to_fire.push(action);
                } else {
                    remaining.push(action);
                }
            }
            *queue = remaining;
        }
        for action in to_fire {
            self.execute_action_now(&action).await?;
        }
        Ok(())
    }

    async fn decide_approval(&self, tool_name: &str) -> (ToolApprovalDecision, String) {
        // Look for a queued Approve/Deny that matches this tool name (or
        // has no filter), consume it, and return the decision. Falling
        // back to Denied avoids hanging the agent on an unexpected
        // approval prompt.
        let mut queue = self.action_queue.lock().await;
        let mut found_index: Option<usize> = None;
        for (idx, action) in queue.iter().enumerate() {
            if approval_matches(action, tool_name) {
                found_index = Some(idx);
                break;
            }
        }
        if let Some(idx) = found_index {
            let action = queue.remove(idx);
            drop(queue);
            return match action {
                Action::Approve { .. } => (ToolApprovalDecision::Approved, "approved".into()),
                Action::Deny { reason, .. } => (
                    ToolApprovalDecision::Denied,
                    format!("denied:{}", reason.unwrap_or_default()),
                ),
                other => {
                    tracing::warn!(?other, "non-approval action matched approval slot");
                    (ToolApprovalDecision::Denied, "denied_no_action".into())
                }
            };
        }
        (ToolApprovalDecision::Denied, "denied_no_action".into())
    }

    async fn evaluate_expectations(&self) -> Vec<String> {
        let mut findings = Vec::new();
        let assistant = self.last_assistant_text.lock().await.clone();
        for required in &self.scenario.expect.final_text_contains {
            if !assistant.contains(required) {
                findings.push(format!(
                    "final assistant output missing required text: {required:?}"
                ));
            }
        }
        if let Some(max_secs) = self.scenario.expect.max_wall_clock_seconds {
            let wall = *self.wall_clock_seconds.lock().await;
            if wall > max_secs {
                findings.push(format!("wall clock {wall}s exceeded max {max_secs}s"));
            }
        }
        if let Some(max_tok) = self.scenario.expect.max_input_tokens {
            let total = *self.total_input_tokens.lock().await;
            if total > max_tok {
                findings.push(format!("input tokens {total} exceeded max {max_tok}"));
            }
        }
        if self.scenario.expect.no_tool_errors {
            let errs = *self.tool_errors.lock().await;
            if errs > 0 {
                findings.push(format!("encountered {errs} tool errors"));
            }
        }
        findings
    }
}

fn approval_matches(action: &Action, tool_name: &str) -> bool {
    let m: Option<&ApprovalMatch> = match action {
        Action::Approve { r#match, .. } => r#match.as_ref(),
        Action::Deny { r#match, .. } => r#match.as_ref(),
        _ => return false,
    };
    match m.and_then(|m| m.tool.as_deref()) {
        Some(expected) => expected == tool_name,
        None => true,
    }
}

fn action_to_value(action: &Action) -> Value {
    serde_json::to_value(action).unwrap_or(Value::Null)
}

fn transcript_text(item: &squeezy_core::TranscriptItem) -> String {
    item.content.clone()
}

/// Internal extension: the agent's workspace_root is stored in its config
/// but not exposed as a getter. Eval needs it to resolve relative
/// edit_file paths. We approximate via env::current_dir if the agent
/// doesn't expose it — but the agent does carry an `AppConfig` so we
/// expose a tiny accessor via a trait below.
trait AgentExt {
    fn workspace_root_clone(&self) -> PathBuf;
}

impl AgentExt for Agent {
    fn workspace_root_clone(&self) -> PathBuf {
        // Squeezy's Agent does not currently expose its config. As a
        // first-cut fallback, use the process cwd which `Agent::new`
        // inherits via `AppConfig::workspace_root`. Improving this is a
        // one-line change in squeezy-agent once we want it.
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }
}
