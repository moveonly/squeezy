use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Mutex as TokioMutex;
use tokio::time::{Duration, sleep, timeout};
use tokio_util::sync::CancellationToken;

use squeezy_agent::{
    Agent, AgentEvent, RequestUserInputResponse, ToolApprovalDecision, ToolOrigin,
};
use squeezy_core::{AppConfig, PermissionMode, SessionMode};
use squeezy_llm::provider_from_config;

use crate::capture::{Capture, EvalEventKind};
use crate::frames::{FrameFinish, FrameRecord, FrameWriter, ToolCallSummary};
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
    /// When true, the driver streams squeezy's activity to stdout as it
    /// happens — assistant text, tool calls, approvals, findings. Set
    /// to false for CI or other unattended runs.
    pub live: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RunOutcome {
    pub run_dir: PathBuf,
    pub trace_event_count: u64,
    pub frame_count: u64,
    pub ticket_count: u64,
    pub findings: Vec<String>,
    pub cost_micro_usd: u64,
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

    // Live narration sink (stdout by default; suppressed in `--quiet`
    // and CI mode). The same printer is also handed to the dispatch
    // loop so it can announce step boundaries before any event fires.
    let live_printer = Arc::new(crate::live::LivePrinter::stdout(options.live));
    let capture = Arc::new(Capture::create_with_live(
        &run_dir,
        Some(live_printer.clone()),
    )?);
    let frames = Arc::new(FrameWriter::create(&run_dir)?);
    if options.live {
        println!(
            "▶ squeezy-eval running: {} ({})",
            scenario.title, scenario.id
        );
    }

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
    let provider_name = agent.provider_name();
    let model = config.model.clone();
    let session_id = agent.session_id().unwrap_or_default();

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
        provider_name,
        model: model.clone(),
        session_id: session_id.clone(),
        total_cost_micro_usd: TokioMutex::new(0),
        live_printer: live_printer.clone(),
    };

    driver.dispatch_steps().await?;

    // 4. Run the auto-finding pattern matchers over the captured trace,
    //    write findings.jsonl, and embed Finding events back into the
    //    trace so triage and downstream tools see them.
    let trace_ctx = crate::findings::TraceContext::load(&capture.path())?;
    let mut findings_log = crate::findings::FindingsLog::create(&run_dir)?;
    let mut auto_findings: Vec<crate::findings::Finding> = Vec::new();
    for rule in crate::findings::default_rules() {
        let hits = rule.check(&trace_ctx, &scenario);
        for finding in &hits {
            findings_log.write(finding)?;
            capture.record(
                None,
                EvalEventKind::Finding {
                    rule_id: finding.rule_id.clone(),
                    severity: finding.severity.as_str().into(),
                    summary: finding.summary.clone(),
                },
            )?;
        }
        auto_findings.extend(hits);
    }
    // Keep the legacy `findings: Vec<String>` for the manifest /
    // RunOutcome shape so existing consumers stay green.
    let legacy_findings: Vec<String> = auto_findings
        .iter()
        .map(|f| format!("[{}] {}", f.rule_id, f.summary))
        .collect();

    // 5. Write the run manifest.
    let trace_event_count = read_line_count(&capture.path())?;
    let frame_count = read_line_count(&frames.path())?;
    let total_cost_micro_usd = *driver.total_cost_micro_usd.lock().await;
    let per_turn_costs = read_per_turn_costs(&frames.path())?;
    let manifest = build_manifest(
        &scenario,
        &options,
        &workspace,
        trace_event_count,
        frame_count,
        &legacy_findings,
        total_cost_micro_usd,
        &per_turn_costs,
        driver.provider_name,
        &driver.model,
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
    let llm_tickets = if triage_enabled {
        match crate::triage::triage(
            &scenario,
            &config,
            &capture.path(),
            &frames.path(),
            &auto_findings,
        )
        .await
        {
            Ok(drafts) => drafts,
            Err(err) => {
                tracing::warn!(error = %err, "triage failed; continuing without tickets");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    if !llm_tickets.is_empty() || !auto_findings.is_empty() {
        let all = synthesize_tickets(llm_tickets, &auto_findings, &scenario);
        ticket_count = all.len() as u64;
        crate::tickets::emit(
            &run_dir,
            &all,
            crate::tickets::EmitOptions {
                emit_github: options.emit_github,
                gh_repo: options.gh_repo.clone(),
                bundle: if driver.session_id.is_empty() {
                    None
                } else {
                    Some(crate::tickets::BundleSource {
                        config: config.clone(),
                        session_id: driver.session_id.clone(),
                    })
                },
            },
        )?;
    }

    live_printer.flush();
    Ok(RunOutcome {
        run_dir,
        trace_event_count,
        frame_count,
        ticket_count,
        findings: legacy_findings,
        cost_micro_usd: total_cost_micro_usd,
    })
}

fn synthesize_tickets(
    mut from_llm: Vec<TicketDraft>,
    findings: &[crate::findings::Finding],
    scenario: &Scenario,
) -> Vec<TicketDraft> {
    for finding in findings {
        from_llm.push(TicketDraft {
            id: finding.rule_id.clone(),
            title: format!(
                "[{}] {}",
                finding.rule_id,
                summarize_first_line(&finding.summary)
            ),
            severity: finding.severity.as_str().into(),
            category: finding.category.clone(),
            summary: finding.summary.clone(),
            repro: format!(
                "Run scenario `{}` and inspect the listed trace events.",
                scenario.id
            ),
            evidence: finding
                .evidence
                .iter()
                .map(|e| crate::tickets::EvidencePointer {
                    trace_event: e.trace_event,
                    frame: e.frame,
                })
                .collect(),
            suggested_fix: None,
        });
    }
    from_llm
}

fn summarize_first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(80).collect()
}

#[allow(clippy::too_many_arguments)]
fn build_manifest(
    scenario: &Scenario,
    options: &RunOptions,
    workspace: &ProvisionedWorkspace,
    trace_event_count: u64,
    frame_count: u64,
    findings: &[String],
    total_cost_micro_usd: u64,
    per_turn_costs: &[(String, u64)],
    provider_name: &str,
    model: &str,
) -> Value {
    json!({
        "schema_version": 2,
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
            crate::workspace::WorkspaceSource::Snapshot { from, sha, worktree } => json!({
                "kind": "snapshot",
                "from": from.display().to_string(),
                "sha": sha,
                "worktree": worktree,
            }),
            crate::workspace::WorkspaceSource::Github { repo, sha } => json!({
                "kind": "github",
                "repo": repo,
                "sha": sha,
            }),
        },
        "provider": provider_name,
        "model": model,
        "totals": {
            "trace_events": trace_event_count,
            "frames": frame_count,
            "findings": findings.len(),
            "cost_micro_usd": total_cost_micro_usd,
            "cost_display": crate::frames::format_cost_micro_usd(total_cost_micro_usd),
        },
        "per_turn_costs": per_turn_costs
            .iter()
            .map(|(turn, micro)| json!({
                "turn_id": turn,
                "cost_micro_usd": micro,
                "cost_display": crate::frames::format_cost_micro_usd(*micro),
            }))
            .collect::<Vec<_>>(),
        "findings": findings,
        "squeezy_version": env!("CARGO_PKG_VERSION"),
    })
}

fn read_per_turn_costs(path: &Path) -> Result<Vec<(String, u64)>, EvalError> {
    use std::io::{BufRead, BufReader};
    let file =
        std::fs::File::open(path).map_err(|err| EvalError::Io(format!("open {path:?}: {err}")))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|err| EvalError::Io(format!("read {path:?}: {err}")))?;
        if line.trim().is_empty() {
            continue;
        }
        let frame: crate::frames::FrameRecord = serde_json::from_str(&line)
            .map_err(|err| EvalError::Internal(format!("parse frame: {err}")))?;
        out.push((frame.turn_id, frame.cost_micro_usd));
    }
    Ok(out)
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
    // Tighten squeezy's live cost broker for probes. AppConfig already
    // has these knobs; they default to permissive values (64 tool calls,
    // 20 MB read, $5 session cap) which lets a planner over-fetch
    // burst slide. Scenarios that probe budget behavior can ratchet
    // them down via the overlay.
    if let Some(v) = overlay.max_tool_calls_per_turn {
        config.max_tool_calls_per_turn = v;
    }
    if let Some(v) = overlay.max_tool_bytes_read_per_turn {
        config.max_tool_bytes_read_per_turn = v;
    }
    if let Some(v) = overlay.max_session_cost_usd_micros {
        config.max_session_cost_usd_micros = Some(v);
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
    provider_name: &'static str,
    model: String,
    /// Session id captured after `Agent::new`. Used by the bug-report
    /// bundling path in `tickets::emit`.
    #[allow(dead_code)]
    session_id: String,
    total_cost_micro_usd: TokioMutex<u64>,
    live_printer: Arc<crate::live::LivePrinter>,
}

impl Driver {
    async fn dispatch_steps(&self) -> Result<(), EvalError> {
        for (idx, step) in self.scenario.steps.clone().into_iter().enumerate() {
            // Announce the step on the live printer so a watching user
            // sees `━━━ step 1: prompt` before any squeezy activity.
            self.live_printer.step(idx, &step);
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
            Action::InjectUserText { text, .. } => {
                self.agent.queue_user_message(text.clone()).await;
                self.capture.record(
                    None,
                    EvalEventKind::ActionStep {
                        action: payload,
                        status: format!("injected:{}", text.chars().take(60).collect::<String>()),
                    },
                )?;
            }
        }
        Ok(())
    }

    async fn dispatch_slash_command(&self, command: &str) -> Result<String, EvalError> {
        let trimmed = command.trim().trim_start_matches('/');
        let (name, args) = trimmed.split_once(' ').unwrap_or((trimmed, ""));
        let outcome = self.agent.dispatch_command(name, args).await;
        let status = match &outcome {
            squeezy_agent::CommandOutcome::Compacted => "compacted".to_string(),
            squeezy_agent::CommandOutcome::ModeChanged { mode, changed } => {
                format!("mode_{mode}_changed={changed}")
            }
            squeezy_agent::CommandOutcome::CostSnapshot { .. } => "cost_snapshot".to_string(),
            squeezy_agent::CommandOutcome::JobsList { count } => format!("jobs_list:{count}"),
            squeezy_agent::CommandOutcome::PermissionsList { count } => {
                format!("permissions_list:{count}")
            }
            squeezy_agent::CommandOutcome::Unsupported { command } => {
                format!("unsupported_slash_command:{command}")
            }
            squeezy_agent::CommandOutcome::Error { message, .. } => format!("error:{message}"),
        };
        Ok(status)
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
                AgentEvent::ToolCallStarted {
                    turn_id,
                    call,
                    origin,
                } => {
                    let turn_str = format!("{turn_id:?}");
                    let value = serde_json::to_value(&call).unwrap_or(Value::Null);
                    received_tool_call = true;
                    *self.total_tool_calls.lock().await += 1;
                    // Push the per-call breadcrumb (name + args preview + hash).
                    // Duplicates are intentionally kept so the auto-findings
                    // rules can detect them at a glance.
                    let summary = ToolCallSummary::from_call(&call.name, &call.arguments);
                    frame.tool_calls.push(summary);
                    self.fire_on_tool_actions(&call.name).await?;
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::ToolCallStarted {
                            call: value,
                            origin: origin_label(origin).to_string(),
                        },
                    )?;
                    // Note: `wait_for: tool_call` is a *signal* only — we
                    // no longer cancel the turn when the tool fires.
                    // Scenarios that want to act mid-stream attach
                    // `when.on_tool = "..."` to the action they want
                    // dispatched concurrently; `fire_on_tool_actions`
                    // above handles that path while the turn keeps
                    // streaming to completion.
                    if let WaitFor::ToolCall { tool } = &wait_for
                        && &call.name == tool
                    {
                        // Record that the gate tripped, then continue.
                        self.capture.record(
                            Some(format!("{turn_id:?}")),
                            EvalEventKind::ActionStep {
                                action: json!({"kind": "wait_for_signal"}),
                                status: format!("tool_call_seen:{tool}"),
                            },
                        )?;
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
                    // Update the matching ToolCallSummary's status. Match by
                    // tool name working backwards so the most recent entry
                    // (the call we just completed) is the one we tag.
                    if let Some(entry) = frame
                        .tool_calls
                        .iter_mut()
                        .rev()
                        .find(|c| c.name == result.tool_name && c.status.is_none())
                    {
                        entry.status = Some(status.to_ascii_lowercase());
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
                    // Emit both the debug rendering (retained for old viewers)
                    // and a structured `summary` field so rule code does not
                    // have to parse the debug string.
                    let value = json!({
                        "debug": format!("{:?}", snapshot),
                        "summary": snapshot.summary,
                        "status": format!("{:?}", snapshot.status),
                    });
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
                AgentEvent::CostWarning { turn_id, status } => {
                    let turn_str = format!("{turn_id:?}");
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::Snapshot {
                            snapshot_kind: "cost_warning".into(),
                            payload: json!({"debug": format!("{:?}", status)}),
                        },
                    )?;
                }
                AgentEvent::CostUpdate {
                    turn_id,
                    tool_count,
                    input_tokens,
                    micro_usd,
                } => {
                    let turn_str = format!("{turn_id:?}");
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::CostUpdate {
                            tool_count,
                            input_tokens,
                            micro_usd,
                        },
                    )?;
                }
                AgentEvent::ToolProgress {
                    turn_id,
                    call_id,
                    tool_name,
                    elapsed_ms,
                } => {
                    let turn_str = format!("{turn_id:?}");
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::ToolProgress {
                            call_id,
                            tool_name,
                            elapsed_ms,
                        },
                    )?;
                }
                AgentEvent::ReasoningDelta { .. } | AgentEvent::ReasoningSegment { .. } => {}
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
                    let cost_micro =
                        squeezy_llm::estimate_cost(self.provider_name, &self.model, &cost)
                            .unwrap_or(0);
                    frame.cost_micro_usd = cost_micro;
                    frame.cost_display = crate::frames::format_cost_micro_usd(cost_micro);
                    *self.total_input_tokens.lock().await += frame.input_tokens;
                    *self.total_cost_micro_usd.lock().await += cost_micro;
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
        // Render the assistant markdown through the TUI's own pipeline so
        // the frame carries both a structured Line/Span representation
        // and an ANSI-escaped string a reviewer can replay.
        let (styled, ansi) = crate::frames::render_styled(&frame.assistant_text);
        frame.styled_lines = styled;
        frame.ansi = ansi;
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

fn origin_label(origin: ToolOrigin) -> &'static str {
    match origin {
        ToolOrigin::Planner => "planner",
        ToolOrigin::Model => "model",
        ToolOrigin::Subagent => "subagent",
    }
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
