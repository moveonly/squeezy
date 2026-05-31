use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Mutex as TokioMutex;
use tokio::time::{Duration, sleep, timeout};
use tokio_util::sync::CancellationToken;

use squeezy_agent::{Agent, AgentEvent, ToolApprovalDecision, ToolOrigin};
use squeezy_core::{AppConfig, PermissionMode, ReasoningEffort, SessionMode};
use squeezy_llm::provider_from_config;

use crate::capture::{Capture, EvalEventKind};
use crate::frames::{FrameFinish, FrameRecord, FrameWriter, ToolCallSummary};
use crate::scenario::{
    Action, ApprovalMatch, Assertion, EditReplace, Scenario, SqueezyOverlay, Step, TranscriptIndex,
    WaitFor,
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
    // Phase 5: optional per-turn TUI render capture. `provision`
    // returns `None` when the scenario didn't opt in, so the default
    // path stays zero-cost.
    let tui_capture =
        crate::tui_capture::TuiCaptureWriter::provision(&run_dir, &scenario.tui_capture)?
            .map(Arc::new);
    if options.live {
        println!(
            "▶ squeezy-eval running: {} ({})",
            scenario.title, scenario.id
        );
    }

    // Phase 7: scenario-scoped env vars. Exported BEFORE workspace
    // provisioning and AppConfig build so providers that depend on
    // env (api keys, SQUEEZY_PROVIDER overrides, MCP server creds)
    // pick them up. The mutation is process-wide; the eval runs one
    // scenario per process today, so blast radius is per-run.
    //
    // SAFETY: documented per-run blast radius. Parallel runners
    // need a separate process per scenario or per-process scoping.
    for (key, value) in &scenario.env_vars {
        unsafe {
            std::env::set_var(key, value);
        }
    }

    // 1. Provision the workspace.
    let scratch_root = options.out_root.join("_workspaces");
    let workspace = workspace::provision(&scenario.workspace, &scratch_root)?;

    // 1b. Materialize any fixture skills under the snapshot's
    // `.squeezy/skills/<dir>/SKILL.md`. Runs before AppConfig builds
    // the SkillsConfig so workspace discovery picks them up.
    materialize_fixture_skills(&workspace.path, &scenario.fixture_skills)?;

    // 2. Build the AppConfig with the scenario overlay applied, then the agent.
    //
    // When the scenario pins a provider (`[squeezy] provider = "..."`), thread it
    // through `from_env_and_settings_with_provider` so the full ProviderConfig
    // (base URLs, api_key_env, transport) is resolved against that preset
    // instead of whatever `SQUEEZY_PROVIDER` happens to be in the eval env.
    // `mock` stays in the standard path because it isn't a real provider preset;
    // the dispatch site below swaps in the MockProvider regardless.
    let provider_override = scenario
        .squeezy
        .provider
        .as_deref()
        .filter(|name| !name.eq_ignore_ascii_case("mock"));
    let mut config = match provider_override {
        Some(provider) => AppConfig::from_env_and_settings_with_provider(provider)
            .map_err(|err| EvalError::Config(format!("load AppConfig: {err}")))?,
        None => AppConfig::from_env_and_settings()
            .map_err(|err| EvalError::Config(format!("load AppConfig: {err}")))?,
    };
    apply_overlay(&mut config, &scenario.squeezy, &workspace.path)?;
    apply_mcp_overlay(&mut config, &scenario.mcp)?;
    let provider = if scenario.squeezy.provider.as_deref() == Some("mock") {
        crate::mock_provider::MockProvider::shared(scenario.mock.clone())
    } else {
        provider_from_config(&config.provider)
            .map_err(|err| EvalError::Provider(provider_hint(err)))?
    };
    let agent = Agent::new(config.clone(), provider.clone());
    let provider_name = agent.provider_name();
    let model = config.model.clone();
    let session_id = agent.session_id().unwrap_or_default();

    // Pre-warm the MCP tool palette so the very first prompt can issue
    // `mcp__*` tool calls without racing the production background
    // refresh. Cheap no-op when the scenario didn't declare any MCP
    // servers (the registry exits immediately on `has_no_enabled_servers`).
    if !config.mcp_servers.is_empty() {
        let outcome = agent.refresh_mcp_tools().await;
        for error in &outcome.errors {
            eprintln!("squeezy-eval: mcp warmup error: {error}");
        }
        capture.record(
            None,
            EvalEventKind::ActionStep {
                action: json!({
                    "kind": "mcp_warmup",
                    "errors": outcome.errors,
                    "ready_servers": outcome.status.per_server.len(),
                }),
                status: if outcome.errors.is_empty() {
                    "ok".into()
                } else {
                    format!("errors:{}", outcome.errors.len())
                },
            },
        )?;
    }

    // 3. Drive the steps. When `drive_tui = true`, build a
    //    `TuiHarness` so prompt steps and the TUI-only actions
    //    (`send_key`, `tui_*` assertions) operate against a live
    //    `TuiApp + Agent + Terminal` instead of just the markdown
    //    capture path.
    let harness = if scenario.tui_capture.drive_tui {
        let width = scenario.tui_capture.width.unwrap_or(160);
        let height = scenario.tui_capture.height.unwrap_or(48);
        let session_mode = config.session_mode;
        // Pin slash-command settings writes (`/theme`, `/statusline`, …)
        // to a scratch file under the per-run dir so the harness can't
        // clobber the operator's real `~/.squeezy/settings.toml` mid-run
        // (squeezy-ramu). The file is created lazily by `apply_edits`
        // when a scenario actually flips a setting.
        let scratch_settings = run_dir.join("scratch-settings.toml");
        let harness = squeezy_tui::testing::TuiHarness::new(
            config.clone(),
            session_mode,
            provider.clone(),
            width,
            height,
            Some(scratch_settings),
        )
        .map_err(|err| EvalError::Internal(format!("init TuiHarness: {err}")))?;
        Some(Arc::new(TokioMutex::new(harness)))
    } else {
        None
    };
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
        last_stop_reason: TokioMutex::new(None),
        observed_tool_calls: TokioMutex::new(Vec::new()),
        task_snapshots: TokioMutex::new(Vec::new()),
        tui_capture: tui_capture.clone(),
        pending_overlays: TokioMutex::new(Vec::new()),
        harness,
        scenario_vars: TokioMutex::new(std::collections::BTreeMap::new()),
    };

    driver.dispatch_steps().await?;

    // Drain the agent's tracked background tasks (MCP tool-palette
    // refresh fired during `start_turn`) before the post-run findings
    // / manifest pass. Without this, parallel `check` runs leave
    // detached spawns alive past their parent task's `tokio::spawn`,
    // and a watchdog SIGKILLs the worker while `run.json` is on disk
    // but the runtime is still waiting on the spawn — stalling the
    // sweep by ~5 minutes per worker.
    driver.agent.shutdown().await;

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
    if let Some(reasoning) = &overlay.reasoning_effort {
        config.reasoning_effort = Some(ReasoningEffort::parse(reasoning).ok_or_else(|| {
            EvalError::Config(format!("unknown reasoning_effort in overlay: {reasoning}"))
        })?);
    }
    if let Some(choice) = &overlay.tool_choice {
        let normalized = choice.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "auto" | "required" | "none" => config.tool_choice = Some(normalized),
            other => {
                return Err(EvalError::Config(format!(
                    "unknown tool_choice in overlay: {other}"
                )));
            }
        }
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
        // Apply uniformly to every capability gate so the scenario's
        // declared permission_mode is comprehensive. Read and
        // ignored_search must be included so internal recovery
        // affordances like `read_tool_output` (used to recover a spilled
        // tool stdout buffer) do not get auto-denied when the operator's
        // settings.toml carries `[permissions] read = "ask"`.
        config.permissions.read = mode;
        config.permissions.edit = mode;
        config.permissions.shell = mode;
        config.permissions.ignored_search = mode;
        config.permissions.web = mode;
        config.permissions.mcp = mode;
    }
    // Provider is now resolved at AppConfig construction time in
    // `run_scenario` via `from_env_and_settings_with_provider`, so it's
    // already baked into `config.provider` by the time `apply_overlay`
    // runs. Nothing further to do here.
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
    if let Some(show) = overlay.show_reasoning_usage {
        config.tui.show_reasoning_usage = show;
    }
    if let Some(enabled) = overlay.checkpoints_enabled {
        config.checkpoints_enabled = enabled;
    }
    if !overlay.excluded_tools.is_empty() {
        for name in &overlay.excluded_tools {
            if !config
                .tools
                .excluded
                .iter()
                .any(|existing| existing == name)
            {
                config.tools.excluded.push(name.clone());
            }
        }
        let excluded: std::collections::BTreeSet<String> =
            config.tools.excluded.iter().cloned().collect();
        config.tools.core.retain(|name| !excluded.contains(name));
        config
            .tools
            .discoverable
            .retain(|name| !excluded.contains(name));
    }
    Ok(())
}

fn materialize_fixture_skills(
    workspace_root: &Path,
    skills: &[crate::scenario::FixtureSkill],
) -> Result<(), EvalError> {
    if skills.is_empty() {
        return Ok(());
    }
    let skills_root = workspace_root.join(".squeezy").join("skills");
    std::fs::create_dir_all(&skills_root)
        .map_err(|err| EvalError::Io(format!("create {skills_root:?}: {err}")))?;
    for skill in skills {
        if skill.dir.is_empty() || skill.dir.chars().any(|c| c == '/' || c == '\\' || c == '.') {
            return Err(EvalError::ScenarioParse(format!(
                "fixture_skill dir must be a simple directory name: {}",
                skill.dir
            )));
        }
        let dir = skills_root.join(&skill.dir);
        std::fs::create_dir_all(&dir)
            .map_err(|err| EvalError::Io(format!("create {dir:?}: {err}")))?;
        let path = dir.join("SKILL.md");
        std::fs::write(&path, &skill.content)
            .map_err(|err| EvalError::Io(format!("write {path:?}: {err}")))?;
    }
    Ok(())
}

fn apply_mcp_overlay(
    config: &mut AppConfig,
    overlay: &crate::scenario::McpScenarioConfig,
) -> Result<(), EvalError> {
    if overlay.servers.is_empty() {
        return Ok(());
    }
    let bundled_fake_mcp = bundled_fake_mcp_path();
    for (name, spec) in &overlay.servers {
        if name.is_empty() {
            return Err(EvalError::ScenarioParse(
                "mcp server name must be non-empty".into(),
            ));
        }
        let transport = match spec.transport.as_deref().unwrap_or("stdio") {
            "stdio" => squeezy_core::McpTransport::Stdio,
            "http" => squeezy_core::McpTransport::Http,
            "sse" => squeezy_core::McpTransport::Sse,
            other => {
                return Err(EvalError::ScenarioParse(format!(
                    "unknown mcp transport: {other}"
                )));
            }
        };
        let command = match spec.command.as_deref() {
            Some("bundled:fake-mcp") => Some(
                bundled_fake_mcp
                    .as_ref()
                    .ok_or_else(|| {
                        EvalError::Config(
                    "scenario requires `bundled:fake-mcp` but its binary was not found alongside \
                     `squeezy-eval`; build with `cargo build -p squeezy-eval`"
                        .into(),
                )
                    })?
                    .display()
                    .to_string(),
            ),
            other => other.map(str::to_string),
        };
        let server = squeezy_core::McpServerConfig {
            enabled: spec.enabled,
            transport,
            command,
            args: spec.args.clone(),
            url: spec.url.clone(),
            timeout_ms: spec.timeout_ms.or(Some(10_000)),
            discovery_timeout_ms: None,
            tool_call_timeout_ms: None,
            enabled_tools: spec.enabled_tools.clone(),
            disabled_tools: Vec::new(),
            env: spec.env.clone(),
            permissions: squeezy_core::McpPermissionConfig::default(),
            bearer_token_env_var: None,
            http_headers: std::collections::BTreeMap::new(),
            env_http_headers: std::collections::BTreeMap::new(),
        };
        config.mcp_servers.insert(name.clone(), server);
    }
    Ok(())
}

/// Look up the sibling `squeezy-fake-mcp` binary that ships with the eval
/// crate. Resolved relative to the running `squeezy-eval` executable so
/// `cargo run -p squeezy-eval` and a release binary both find the
/// fixture in their own `target/<profile>/` directory.
fn bundled_fake_mcp_path() -> Option<PathBuf> {
    let exe_name = if cfg!(windows) {
        "squeezy-fake-mcp.exe"
    } else {
        "squeezy-fake-mcp"
    };
    let current = std::env::current_exe().ok()?;
    let dir = current.parent()?;
    let candidate = dir.join(exe_name);
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
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
    /// Stop reason of the most recently completed turn. Drives the
    /// Phase 2 `Assertion::StopReason` evaluator.
    last_stop_reason: TokioMutex<Option<squeezy_llm::StopReason>>,
    /// Per-turn breadcrumb of `(name, arguments_json)` for every
    /// `ToolCallStarted` event observed during the run. Drives
    /// `Assertion::ToolCallWithArgs`. Keyed by turn id is unnecessary
    /// here — the assertion is "any tool call so far".
    observed_tool_calls: TokioMutex<Vec<(String, Value)>>,
    /// Every `TaskStateSnapshot` the agent emitted, in arrival order.
    /// Drives `Assertion::TaskStateContains`.
    task_snapshots: TokioMutex<Vec<squeezy_core::TaskStateSnapshot>>,
    /// Phase 5 TUI render-capture writer. `None` when the scenario
    /// didn't opt in via `[tui_capture] enabled = true`.
    tui_capture: Option<Arc<crate::tui_capture::TuiCaptureWriter>>,
    /// Overlays opened during the in-flight turn, populated as
    /// approval / elicitation / user-input events arrive. Cleared at
    /// turn-start and emitted into the next `TuiFrame`.
    pending_overlays: TokioMutex<Vec<crate::tui_capture::TuiOverlayEvent>>,
    /// Live `TuiApp` driver. Some when `[tui_capture] drive_tui =
    /// true`; the driver routes prompt steps and TUI-driving actions
    /// through it instead of through `agent` directly. None for the
    /// default markdown-only render path.
    harness: Option<Arc<TokioMutex<squeezy_tui::testing::TuiHarness>>>,
    /// Scenario-local variable bag populated by
    /// `Action::CaptureSessionId` and consumed by `${var}`
    /// substitution in slash-command strings. squeezy-wtxu (audit H4).
    scenario_vars: TokioMutex<std::collections::BTreeMap<String, String>>,
}

/// Replace `${var}` occurrences in `text` with the matching entry
/// from `vars`. Unknown vars are left in place — the caller decides
/// whether to error or pass through.
fn substitute_scenario_vars(
    text: &str,
    vars: &std::collections::BTreeMap<String, String>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next();
            let mut name = String::new();
            let mut closed = false;
            for next in chars.by_ref() {
                if next == '}' {
                    closed = true;
                    break;
                }
                name.push(next);
            }
            if closed && let Some(value) = vars.get(&name) {
                out.push_str(value);
                continue;
            }
            out.push_str("${");
            out.push_str(&name);
            if closed {
                out.push('}');
            }
        } else {
            out.push(c);
        }
    }
    out
}

impl Driver {
    async fn dispatch_steps(&self) -> Result<(), EvalError> {
        for (idx, step) in self.scenario.steps.clone().into_iter().enumerate() {
            // Announce the step on the live printer so a watching user
            // sees `━━━ step 1: prompt` before any squeezy activity.
            self.live_printer.step(idx, &step);
            // Phase 6: persist step boundaries into the trace so a
            // post-hoc replay can reconstruct the scenario shape
            // (previously this was stdout-only on the live printer,
            // making the artifact incomplete).
            let step_kind = match &step {
                Step::Prompt { .. } => "prompt".to_string(),
                Step::Action(action) => format!("action:{}", action_kind_label(action)),
            };
            self.capture.record(
                None,
                EvalEventKind::ActionStep {
                    action: json!({
                        "kind": "step_boundary",
                        "index": idx + 1,
                        "step_kind": step_kind,
                    }),
                    status: "started".into(),
                },
            )?;
            match step {
                Step::Prompt { text, wait_for } => {
                    if self.harness.is_some() {
                        self.run_prompt_through_harness(text).await?;
                    } else {
                        self.run_prompt(text, wait_for).await?;
                    }
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
                // ${var} substitution from prior `capture_session_id`
                // (and any future capture_*) so chained scenarios can
                // build the slash text from runtime-only values.
                let resolved = {
                    let vars = self.scenario_vars.lock().await;
                    substitute_scenario_vars(command, &vars)
                };
                let status = self.dispatch_slash_command(&resolved).await?;
                self.capture.record(
                    None,
                    EvalEventKind::SlashCommand {
                        command: resolved.clone(),
                    },
                )?;
                let mut payload = payload;
                if resolved != *command
                    && let Some(obj) = payload.as_object_mut()
                {
                    obj.insert("command".into(), Value::from(resolved));
                }
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
            Action::RespondElicitation { .. } | Action::RespondUserInput { .. } => {
                // Same queue-then-consume pattern as Approve/Deny. The
                // McpElicitationRequested / RequestUserInputRequested
                // handler in `run_prompt` pops a matching action and
                // sends its decision through the agent's response_tx.
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
            Action::ApplyDiff {
                path, unified_diff, ..
            } => {
                let status = self.apply_unified_diff(path, unified_diff)?;
                self.capture.record(
                    None,
                    EvalEventKind::ActionStep {
                        action: payload,
                        status,
                    },
                )?;
            }
            Action::SwitchMode { mode, .. } => {
                let normalized = mode.trim().to_ascii_lowercase();
                let status = match normalized.as_str() {
                    "plan" => self.dispatch_slash_command("/plan").await?,
                    "build" => self.dispatch_slash_command("/build").await?,
                    other => format!("asserted_fail: unknown mode {other:?}"),
                };
                self.capture.record(
                    None,
                    EvalEventKind::ActionStep {
                        action: payload,
                        status,
                    },
                )?;
            }
            Action::AttachFile { path, .. } => {
                let resolved = if path.is_absolute() {
                    path.clone()
                } else {
                    self.agent.as_ref().workspace_root_clone().join(path)
                };
                let status = match self.agent.attach_file_context(resolved.clone()).await {
                    Ok(update) => format!(
                        "attached:id={} bytes={} status={}",
                        update.attachment.id,
                        update.attachment.stored_bytes,
                        update.attachment.status.as_str()
                    ),
                    Err(err) => format!("asserted_fail: attach_file: {err}"),
                };
                self.capture.record(
                    None,
                    EvalEventKind::ActionStep {
                        action: payload,
                        status,
                    },
                )?;
            }
            Action::SendKey { key, .. } => {
                let status = self.send_harness_key(key.as_str()).await;
                self.capture.record(
                    None,
                    EvalEventKind::ActionStep {
                        action: payload,
                        status,
                    },
                )?;
            }
            Action::InjectMcpElicitation { request, .. } => {
                let status = self.inject_mcp_elicitation_into_harness(request).await;
                self.capture.record(
                    None,
                    EvalEventKind::ActionStep {
                        action: payload,
                        status,
                    },
                )?;
            }
            Action::SendKeys { keys, delay_ms, .. } => {
                let status = self.send_harness_keys(keys, *delay_ms).await;
                self.capture.record(
                    None,
                    EvalEventKind::ActionStep {
                        action: payload,
                        status,
                    },
                )?;
            }
            Action::DetachAttachment { id, .. } => {
                let status = match self.agent.detach_context_attachment(id).await {
                    Ok(attachment) => format!("detached:id={}", attachment.id),
                    Err(err) => format!("asserted_fail: detach_attachment: {err}"),
                };
                self.capture.record(
                    None,
                    EvalEventKind::ActionStep {
                        action: payload,
                        status,
                    },
                )?;
            }
            Action::CaptureSessionId { var, .. } => {
                let status = match self.agent.session_id() {
                    Some(id) => {
                        self.scenario_vars
                            .lock()
                            .await
                            .insert(var.clone(), id.clone());
                        format!("captured_session_id:var={var}:id={id}")
                    }
                    None => {
                        "asserted_fail: capture_session_id: agent has no session id yet".to_string()
                    }
                };
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
        // When the harness is live (drive_tui = true), route the slash
        // through the TUI's `handle_slash_command` so the visual side
        // (config_screen toggles, overlay::Overlay openings, status
        // updates, transcript pushes via `toggle_*`/`handle_slash_*`)
        // actually runs. The agent-side `dispatch_command_raw` path
        // only fires for commands that are pure agent-state changes
        // (e.g. `/attach`, `/pin`, `/undo`) — and for those the TUI's
        // `apply_dispatch_command` calls into the agent helpers
        // directly, so we don't double-dispatch.
        if let Some(harness) = self.harness.as_ref() {
            let routed = {
                let mut h = harness.lock().await;
                h.dispatch_slash_command(command)
                    .await
                    .map_err(|err| EvalError::Internal(format!("harness slash: {err}")))?
            };
            let status_text = {
                let h = harness.lock().await;
                h.status_text().to_string()
            };
            return Ok(format!(
                "tui_dispatched:routed={routed}:status={status_text:?}"
            ));
        }
        let outcome = self.agent.dispatch_command_raw(command).await;
        let status = match &outcome {
            squeezy_agent::DispatchOutcome::Compacted { skipped } => {
                format!("compacted:skipped={skipped}")
            }
            squeezy_agent::DispatchOutcome::CompactedUndo { restored } => {
                format!("compact_undo:restored={restored}")
            }
            squeezy_agent::DispatchOutcome::ModeChanged {
                mode,
                changed,
                prompt,
            } => {
                let prompt_marker = prompt
                    .as_deref()
                    .map(|p| format!(":prompt_len={}", p.len()))
                    .unwrap_or_default();
                format!("mode_{mode}_changed={changed}{prompt_marker}")
            }
            squeezy_agent::DispatchOutcome::CostSnapshot { .. } => "cost_snapshot".to_string(),
            squeezy_agent::DispatchOutcome::ContextSnapshot { .. } => {
                "context_snapshot".to_string()
            }
            squeezy_agent::DispatchOutcome::ReviewerSnapshot { count } => {
                format!("reviewer_snapshot:{count}")
            }
            squeezy_agent::DispatchOutcome::JobsList { count } => format!("jobs_list:{count}"),
            squeezy_agent::DispatchOutcome::TaskDetail { id, found } => {
                format!("task_detail:{id}:found={found}")
            }
            squeezy_agent::DispatchOutcome::TaskCancel { id, cancelled } => {
                format!("task_cancel:{id}:cancelled={cancelled}")
            }
            squeezy_agent::DispatchOutcome::PermissionsList { count } => {
                format!("permissions_list:{count}")
            }
            squeezy_agent::DispatchOutcome::Forked { new_session_id } => {
                format!("forked:{new_session_id}")
            }
            squeezy_agent::DispatchOutcome::SessionsList { count } => {
                format!("sessions_list:{count}")
            }
            squeezy_agent::DispatchOutcome::SessionDetail { session_id, exists } => {
                format!("session_detail:{session_id}:exists={exists}")
            }
            squeezy_agent::DispatchOutcome::SessionExported { session_id, bytes } => {
                format!("session_exported:{session_id}:bytes={bytes}")
            }
            squeezy_agent::DispatchOutcome::SessionExportedHtml {
                session_id,
                path,
                bytes,
            } => format!("session_exported_html:{session_id}:path={path}:bytes={bytes}"),
            squeezy_agent::DispatchOutcome::SessionCleanup { archived, removed } => {
                format!("session_cleanup:archived={archived}:removed={removed}")
            }
            squeezy_agent::DispatchOutcome::Attached { id } => format!("attached:{id}"),
            squeezy_agent::DispatchOutcome::Detached { id } => format!("detached:{id}"),
            squeezy_agent::DispatchOutcome::AttachmentsList { count } => {
                format!("attachments_list:{count}")
            }
            squeezy_agent::DispatchOutcome::Pinned { id } => format!("pinned:{id}"),
            squeezy_agent::DispatchOutcome::Unpinned { id } => format!("unpinned:{id}"),
            squeezy_agent::DispatchOutcome::PinsList { count } => format!("pins_list:{count}"),
            squeezy_agent::DispatchOutcome::DiffSnapshot {
                vcs_kind,
                files_changed,
                additions,
                deletions,
                untracked_files,
                ..
            } => format!(
                "diff_snapshot:vcs={vcs_kind}:files={files_changed}:+{additions}-{deletions}:untracked={untracked_files}"
            ),
            // `None` is the checkpoints-disabled path (no store wired
            // up); `Some(_)` with `applied=false, skipped=true` is the
            // clean-tree path where rollback found no journal entry.
            squeezy_agent::DispatchOutcome::CheckpointUndo {
                applied,
                skipped,
                checkpoint_ids,
                result,
            } => match result {
                None => "checkpoint_undo:disabled".to_string(),
                Some(_) => format!(
                    "checkpoint_undo:applied={applied}:skipped={skipped}:checkpoints={}",
                    checkpoint_ids.join(",")
                ),
            },
            squeezy_agent::DispatchOutcome::TuiOnly { command } => {
                // Reached only when `drive_tui = false`: nothing more
                // to do — the command requires a live TUI to take
                // visual effect, and the scenario opted out.
                format!("tui_only:{command}")
            }
            squeezy_agent::DispatchOutcome::Unsupported { command } => {
                format!("unsupported_slash_command:{command}")
            }
            squeezy_agent::DispatchOutcome::Error { message, .. } => format!("error:{message}"),
            squeezy_agent::DispatchOutcome::SessionRenamed { display_name, .. } => {
                format!("session_renamed:{}", display_name.as_deref().unwrap_or(""))
            }
            squeezy_agent::DispatchOutcome::SessionLabelled { labels, .. } => {
                format!("session_labelled:{}", labels.join(","))
            }
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
            Assertion::ToolCallWithArgs {
                tool,
                args_contains,
            } => {
                let observed = self.observed_tool_calls.lock().await;
                let hit = observed.iter().any(|(name, args)| {
                    name == tool
                        && serde_json::to_string(args)
                            .map(|s| s.contains(args_contains))
                            .unwrap_or(false)
                });
                if hit {
                    "asserted_pass".into()
                } else {
                    format!(
                        "asserted_fail: no `{tool}` tool call carrying args containing {args_contains:?}"
                    )
                }
            }
            Assertion::FindingFired { rule_id } => {
                // The findings scan runs after dispatch completes, so
                // we record a deferred marker. The Phase 2 follow-up
                // (or a small in-line check in `run_scenario` after
                // `default_rules` runs) re-evaluates these markers
                // and emits a Finding when the named rule didn't
                // fire. Until then the status carries the request
                // so triage can spot pending checks.
                format!("deferred_finding_fired:{rule_id}")
            }
            Assertion::StopReason { equals, not_in } => {
                let actual = self.last_stop_reason.lock().await.clone();
                let actual_label = actual.as_ref().map(|r| match r {
                    squeezy_llm::StopReason::EndTurn => "end_turn".to_string(),
                    squeezy_llm::StopReason::ToolUse => "tool_use".to_string(),
                    squeezy_llm::StopReason::MaxTokens => "max_tokens".to_string(),
                    squeezy_llm::StopReason::ContextWindowExceeded => {
                        "context_window_exceeded".to_string()
                    }
                    squeezy_llm::StopReason::StopSequence => "stop_sequence".to_string(),
                    squeezy_llm::StopReason::Refusal => "refusal".to_string(),
                    squeezy_llm::StopReason::Other(other) => other.clone(),
                });
                if let Some(expected) = equals
                    && actual_label.as_deref() != Some(expected.as_str())
                {
                    return format!(
                        "asserted_fail: stop_reason={actual_label:?} expected {expected:?}"
                    );
                }
                if let Some(label) = actual_label.as_ref()
                    && not_in.iter().any(|forbidden| forbidden == label)
                {
                    return format!(
                        "asserted_fail: stop_reason={label:?} in forbidden set {not_in:?}"
                    );
                }
                "asserted_pass".into()
            }
            Assertion::TaskStateContains {
                step_matches,
                blocker_contains,
            } => {
                let snapshots = self.task_snapshots.lock().await;
                if snapshots.is_empty() {
                    return "asserted_fail: no task_state_updated snapshots observed".into();
                }
                if let Some(needle) = step_matches.as_deref() {
                    let hit = snapshots.iter().any(|snap| {
                        snap.steps.iter().any(|step| {
                            step.title.contains(needle)
                                || step
                                    .detail
                                    .as_deref()
                                    .map(|d| d.contains(needle))
                                    .unwrap_or(false)
                        })
                    });
                    if !hit {
                        return format!(
                            "asserted_fail: no task_state step title/detail contains {needle:?}"
                        );
                    }
                }
                if let Some(needle) = blocker_contains.as_deref() {
                    let hit = snapshots.iter().any(|snap| {
                        snap.blocker
                            .as_deref()
                            .map(|b| b.contains(needle))
                            .unwrap_or(false)
                    });
                    if !hit {
                        return format!("asserted_fail: no task_state blocker contains {needle:?}");
                    }
                }
                "asserted_pass".into()
            }
            Assertion::TuiStatusContains { text } => self.assert_tui_status_contains(text).await,
            Assertion::TuiTranscriptEntry {
                index,
                entry_kind,
                collapsed,
            } => {
                self.assert_tui_transcript_entry(index, entry_kind.as_deref(), *collapsed)
                    .await
            }
            Assertion::TuiFrameContains { text } => self.assert_tui_frame_contains(text).await,
            Assertion::TuiFrameDoesNotContain { text } => {
                self.assert_tui_frame_does_not_contain(text).await
            }
            Assertion::TuiCellLuminanceLe {
                max,
                channel,
                region,
            } => {
                self.assert_tui_cell_luminance_le(*max, channel.as_deref(), region.as_ref())
                    .await
            }
            Assertion::ModalActive { name } => self.assert_modal_active(name).await,
            Assertion::ConfigScreenSection { name } => self.assert_config_section(name).await,
            Assertion::ActionStepStatusContains { command, contains } => {
                self.assert_action_step_status_contains(command.as_deref(), contains)
            }
        }
    }

    async fn assert_config_section(&self, name: &str) -> String {
        let Some(harness) = self.harness.as_ref() else {
            return "asserted_fail: config_screen_section requires [tui_capture] drive_tui = true"
                .into();
        };
        let h = harness.lock().await;
        let actual = h.config_section();
        let trimmed = name.trim();
        match actual {
            Some(slug) if slug.eq_ignore_ascii_case(trimmed) => "asserted_pass".into(),
            Some(slug) => {
                format!("asserted_fail: expected config_screen section {trimmed:?}, got {slug:?}")
            }
            None => format!(
                "asserted_fail: expected config_screen section {trimmed:?}, but no config_screen is open"
            ),
        }
    }

    fn assert_action_step_status_contains(
        &self,
        command_filter: Option<&str>,
        needle: &str,
    ) -> String {
        match self.capture.last_slash_status(command_filter) {
            Some((_, status)) if status.contains(needle) => "asserted_pass".into(),
            Some((cmd, status)) => format!(
                "asserted_fail: latest slash {cmd:?} status {status:?} does not contain {needle:?}"
            ),
            None => match command_filter {
                Some(cmd) => {
                    format!("asserted_fail: no slash_command action step recorded for {cmd:?}")
                }
                None => "asserted_fail: no slash_command action step recorded".into(),
            },
        }
    }

    async fn assert_modal_active(&self, name: &str) -> String {
        let Some(harness) = self.harness.as_ref() else {
            return "asserted_fail: modal_active requires [tui_capture] drive_tui = true".into();
        };
        let h = harness.lock().await;
        let current = h.current_modal();
        let trimmed = name.trim();
        let expect_none = trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none");
        match (expect_none, current) {
            (true, None) => "asserted_pass".into(),
            (true, Some(actual)) => {
                format!("asserted_fail: expected no modal, got {actual:?}")
            }
            (false, Some(actual)) if actual.eq_ignore_ascii_case(trimmed) => "asserted_pass".into(),
            (false, actual) => format!(
                "asserted_fail: expected modal {trimmed:?}, got {:?}",
                actual.unwrap_or("<none>")
            ),
        }
    }

    async fn assert_tui_frame_does_not_contain(&self, text: &str) -> String {
        let Some(harness) = self.harness.as_ref() else {
            return "asserted_fail: tui_frame_does_not_contain requires [tui_capture] drive_tui = true".into();
        };
        let mut h = harness.lock().await;
        match h.render_frame() {
            Ok(frame) => {
                if !frame.plain_text.contains(text) {
                    "asserted_pass".into()
                } else {
                    let preview: String = frame
                        .plain_text
                        .lines()
                        .map(|l| l.trim_end())
                        .filter(|l| {
                            !l.is_empty()
                                && !l
                                    .chars()
                                    .all(|c| matches!(c, '╭' | '╰' | '│' | '─' | '╮' | '╯' | ' '))
                        })
                        .collect::<Vec<_>>()
                        .join(" / ");
                    let preview = preview.chars().take(900).collect::<String>();
                    format!("asserted_fail: frame still contains {text:?} · preview: {preview}")
                }
            }
            Err(err) => format!("asserted_fail: render: {err}"),
        }
    }

    async fn run_prompt_through_harness(&self, prompt: String) -> Result<(), EvalError> {
        let Some(harness) = self.harness.as_ref() else {
            return Err(EvalError::Internal(
                "run_prompt_through_harness called without a harness".into(),
            ));
        };
        let turn_start = Instant::now();
        let mut h = harness.lock().await;
        h.start_user_turn(prompt.clone());

        // Cap the number of approval modals we'll route through one
        // prompt. Without a cap a buggy provider that re-emits the
        // same ApprovalRequested forever would re-trigger our loop
        // forever. 64 is well above any wave-2 probe's tool-call
        // count and keeps the failure mode an `Err(_)` rather than a
        // silent hang.
        const MAX_APPROVAL_HOPS: usize = 64;
        let mut approval_hops = 0usize;
        loop {
            h.pump_until_idle()
                .await
                .map_err(|err| EvalError::Internal(format!("harness pump: {err}")))?;
            let Some(tool_name) = h.pending_approval_tool().map(str::to_string) else {
                break;
            };
            // Mirror Driver::decide_approval's queue protocol: pop the
            // matching Approve/Deny action and route it back into the
            // harness via the new respond_* helpers. Falling through to
            // Deny when nothing matches keeps drive_tui parity with the
            // non-drive_tui path (where decide_approval returns Denied
            // on an empty queue).
            let (decision, recorded) = self.decide_approval(&tool_name).await;
            let routed = match decision {
                ToolApprovalDecision::Approved
                | ToolApprovalDecision::AllowOnce
                | ToolApprovalDecision::AllowSession
                | ToolApprovalDecision::AllowRuleUser
                | ToolApprovalDecision::AllowRuleProject => h.respond_approval(),
                _ => h.respond_deny(),
            };
            self.capture.record(
                None,
                EvalEventKind::Approval {
                    request: json!({ "tool": tool_name.clone() }),
                    decision: format!("{recorded}{}", if routed { "" } else { ":unrouted" }),
                },
            )?;
            approval_hops += 1;
            if approval_hops >= MAX_APPROVAL_HOPS {
                return Err(EvalError::Internal(format!(
                    "harness pump: exceeded {MAX_APPROVAL_HOPS} approval modals on one prompt",
                )));
            }
        }

        // Synthesize a turn id for capture / frame parity. The TUI's
        // own `TurnId` is consumed inside `drain_agent_events` and never
        // surfaces back to the harness, so we mint a deterministic
        // `harness-<n>` label off the existing turn counter that
        // already drives `last_turn_id` for the non-drive_tui path.
        let assistant_text = h.last_assistant_text();
        let status_text = h.status_text().to_string();
        let transcript_count = h.transcript_entries().len();
        drop(h);

        let prior_turn = self.last_turn_id.lock().await.clone();
        let next_index = prior_turn
            .as_deref()
            .and_then(|s| s.strip_prefix("harness-"))
            .and_then(|n| n.parse::<u64>().ok())
            .map(|n| n + 1)
            .unwrap_or(1);
        let turn_str = format!("harness-{next_index}");
        *self.last_turn_id.lock().await = Some(turn_str.clone());

        // Mirror the run_prompt path so findings rules and downstream
        // tools see the same shape under drive_tui = true. Order
        // matters: TurnStarted → AssistantDelta → TurnCompleted so the
        // findings.rs trace walker fills `per_turn_text` and
        // `last_completed_turn`.
        self.capture
            .record(Some(turn_str.clone()), EvalEventKind::TurnStarted)?;
        if !assistant_text.is_empty() {
            self.capture.record(
                Some(turn_str.clone()),
                EvalEventKind::AssistantDelta {
                    delta: assistant_text.clone(),
                },
            )?;
        }
        self.capture.record(
            Some(turn_str.clone()),
            EvalEventKind::TurnCompleted {
                metrics: Value::Null,
                cost: Value::Null,
                stop_reason: None,
                reasoning_only_stop: false,
                message: None,
                response_id: None,
                context_estimate: None,
            },
        )?;
        self.capture.record(
            None,
            EvalEventKind::ActionStep {
                action: json!({"kind": "harness_prompt", "text": prompt}),
                status: format!(
                    "drained · {transcript_count} transcript entries · status={status_text:?}",
                ),
            },
        )?;

        // Also push the assistant text into `last_assistant_text` so
        // `Assertion::TextContains` works under the harness path. The
        // run_prompt path does the same via the AssistantDelta event
        // handler.
        {
            let mut lat = self.last_assistant_text.lock().await;
            lat.clear();
            lat.push_str(&assistant_text);
        }

        // Build a FrameRecord for frames.jsonl + a TuiFrame for
        // frames_tui.jsonl. Without these, expect_final_text_contains
        // false-positives on every drive_tui scenario (squeezy-bnz) and
        // `view` / `replay.tui` report zero rows.
        let mut frame = FrameRecord {
            turn_id: turn_str.clone(),
            prompt: prompt.clone(),
            assistant_text: assistant_text.clone(),
            elapsed_ms: turn_start.elapsed().as_millis() as u64,
            ..Default::default()
        };
        let (styled, ansi) = crate::frames::render_styled(&frame.assistant_text);
        frame.styled_lines = styled;
        frame.ansi = ansi;
        self.frames.write(&frame)?;

        if let Some(writer) = self.tui_capture.as_ref() {
            let overlays = self.pending_overlays.lock().await.clone();
            let rendered = crate::tui_capture::render_capture_to_grid(
                &frame.assistant_text,
                &overlays,
                writer.width(),
                writer.height(),
            )?;
            let tui_frame = crate::tui_capture::TuiFrame {
                turn_id: frame.turn_id.clone(),
                width: writer.width(),
                height: writer.height(),
                cells: rendered.cells,
                plain_text: rendered.plain_text,
                ansi: rendered.ansi,
                visual_truncated: rendered.visual_truncated,
                omitted_line_count: rendered.omitted_line_count,
                overlays,
                trigger: Some(crate::tui_capture::TuiFrameTrigger {
                    kind: "turn_completed".into(),
                    step_index: None,
                    key: None,
                }),
                transcript: Vec::new(),
                status_text: Some(status_text),
            };
            writer.write(&tui_frame)?;
        }
        *self.wall_clock_seconds.lock().await = self.run_start.elapsed().as_secs();
        Ok(())
    }

    async fn send_harness_key(&self, key: &str) -> String {
        let Some(harness) = self.harness.as_ref() else {
            return "asserted_fail: send_key requires [tui_capture] drive_tui = true".into();
        };
        let Some(event) = squeezy_tui::testing::parse_key(key) else {
            return format!("asserted_fail: unparseable key spec {key:?}");
        };
        let mut h = harness.lock().await;
        match h.send_key(event).await {
            Ok(_) => format!("sent {key} · status={:?}", h.status_text()),
            Err(err) => format!("asserted_fail: send_key {key}: {err}"),
        }
    }

    async fn inject_mcp_elicitation_into_harness(
        &self,
        request: &crate::scenario::InjectedMcpElicitation,
    ) -> String {
        let Some(harness) = self.harness.as_ref() else {
            return "asserted_fail: inject_mcp_elicitation requires \
                    [tui_capture] drive_tui = true"
                .into();
        };
        let kind = match request
            .kind
            .as_deref()
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("url") => squeezy_tools::McpElicitationKind::Url,
            // Default + explicit "form": modal/Form is the path the
            // wave-1 finding flagged as untestable.
            _ => squeezy_tools::McpElicitationKind::Form,
        };
        let req = squeezy_tui::testing::TuiHarness::make_mcp_elicitation_request(
            request.server.clone(),
            kind,
            request.message.clone(),
            request.schema.clone(),
            request.url.clone(),
        );
        let mut h = harness.lock().await;
        let _rx = h.push_pending_mcp_elicitation(req);
        // Drop the receiver: the harness drives the modal via `send_key`,
        // and the response payload is observed through the TuiApp state
        // (transcript additions, status updates) rather than the oneshot
        // channel.
        format!(
            "injected_mcp_elicitation:server={} status={:?}",
            request.server,
            h.status_text()
        )
    }

    async fn send_harness_keys(&self, keys: &[String], delay_ms: u64) -> String {
        let Some(harness) = self.harness.as_ref() else {
            return "asserted_fail: send_keys requires [tui_capture] drive_tui = true".into();
        };
        let mut events = Vec::with_capacity(keys.len());
        for spec in keys {
            match squeezy_tui::testing::parse_key(spec) {
                Some(ev) => events.push((spec.clone(), ev)),
                None => return format!("asserted_fail: unparseable key spec {spec:?}"),
            }
        }
        let mut h = harness.lock().await;
        for (spec, event) in &events {
            if let Err(err) = h.send_key(*event).await {
                return format!("asserted_fail: send_key {spec}: {err}");
            }
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }
        format!("sent {} keys · status={:?}", events.len(), h.status_text())
    }

    async fn assert_tui_status_contains(&self, text: &str) -> String {
        let Some(harness) = self.harness.as_ref() else {
            return "asserted_fail: tui_status_contains requires [tui_capture] drive_tui = true"
                .into();
        };
        let h = harness.lock().await;
        let status = h.status_text();
        if status.contains(text) {
            "asserted_pass".into()
        } else {
            format!("asserted_fail: status={status:?} does not contain {text:?}")
        }
    }

    async fn assert_tui_transcript_entry(
        &self,
        index: &TranscriptIndex,
        entry_kind: Option<&str>,
        collapsed: Option<bool>,
    ) -> String {
        let Some(harness) = self.harness.as_ref() else {
            return "asserted_fail: tui_transcript_entry requires [tui_capture] drive_tui = true"
                .into();
        };
        let h = harness.lock().await;
        let entries = h.transcript_entries();
        let resolved = match index {
            TranscriptIndex::Last => entries.last().map(|e| (entries.len() - 1, e.clone())),
            TranscriptIndex::LastOfKind { entry_kind: kind } => entries
                .iter()
                .enumerate()
                .rev()
                .find(|(_, e)| e.kind == kind)
                .map(|(i, e)| (i, e.clone())),
            TranscriptIndex::Absolute { index } => entries.get(*index).map(|e| (*index, e.clone())),
        };
        let Some((position, entry)) = resolved else {
            return format!(
                "asserted_fail: no transcript entry matches {index:?} (have {} entries: {:?})",
                entries.len(),
                entries
                    .iter()
                    .map(|e| (e.kind, e.collapsed))
                    .collect::<Vec<_>>()
            );
        };
        if let Some(expected_kind) = entry_kind
            && entry.kind != expected_kind
        {
            return format!(
                "asserted_fail: entry[{position}].kind={:?} expected {expected_kind:?}",
                entry.kind
            );
        }
        if let Some(expected_collapsed) = collapsed
            && entry.collapsed != expected_collapsed
        {
            return format!(
                "asserted_fail: entry[{position}].collapsed={} expected {expected_collapsed}",
                entry.collapsed
            );
        }
        "asserted_pass".into()
    }

    async fn assert_tui_frame_contains(&self, text: &str) -> String {
        let Some(harness) = self.harness.as_ref() else {
            return "asserted_fail: tui_frame_contains requires [tui_capture] drive_tui = true"
                .into();
        };
        let mut h = harness.lock().await;
        match h.render_frame() {
            Ok(frame) => {
                if frame.plain_text.contains(text) {
                    "asserted_pass".into()
                } else {
                    // Compress each line: trim trailing spaces; skip
                    // empty / box-drawing-only lines so the preview
                    // shows transcript content, not the startup card
                    // border.
                    let preview: String = frame
                        .plain_text
                        .lines()
                        .map(|l| l.trim_end())
                        .filter(|l| {
                            !l.is_empty()
                                && !l
                                    .chars()
                                    .all(|c| matches!(c, '╭' | '╰' | '│' | '─' | '╮' | '╯' | ' '))
                        })
                        .collect::<Vec<_>>()
                        .join(" / ");
                    let preview = preview.chars().take(700).collect::<String>();
                    let entries = h.transcript_entries();
                    let entry_dump = entries
                        .iter()
                        .map(|e| format!("[{}|col={}|{:.30?}]", e.kind, e.collapsed, e.preview))
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!(
                        "asserted_fail: frame does not contain {text:?} · entries: {entry_dump} · preview: {preview}"
                    )
                }
            }
            Err(err) => format!("asserted_fail: render: {err}"),
        }
    }

    async fn assert_tui_cell_luminance_le(
        &self,
        max: u8,
        channel: Option<&str>,
        region: Option<&crate::scenario::CellRegion>,
    ) -> String {
        let Some(harness) = self.harness.as_ref() else {
            return "asserted_fail: tui_cell_luminance_le requires [tui_capture] drive_tui = true"
                .into();
        };
        let channel = channel.unwrap_or("fg");
        if channel != "fg" && channel != "bg" {
            return format!(
                "asserted_fail: tui_cell_luminance_le channel must be \"fg\" or \"bg\", got {channel:?}"
            );
        }
        let mut h = harness.lock().await;
        let frame = match h.render_frame() {
            Ok(f) => f,
            Err(err) => return format!("asserted_fail: render: {err}"),
        };
        // Clip the requested region to the actual frame dimensions so a
        // scenario authored against width=160 doesn't fail when the
        // harness rebuilds at a different size.
        let (x0, y0, x1, y1) = match region {
            Some(r) => (
                r.x0,
                r.y0,
                r.x1.min(frame.width.saturating_sub(1)),
                r.y1.min(frame.height.saturating_sub(1)),
            ),
            None => (
                0,
                0,
                frame.width.saturating_sub(1),
                frame.height.saturating_sub(1),
            ),
        };
        // Worst-offending cell: (x, y, colour-name, luminance, rgb).
        type WorstCell = (u16, u16, String, u8, (u8, u8, u8));
        let mut worst: Option<WorstCell> = None;
        for cell in frame.cells.iter() {
            if cell.x < x0 || cell.x > x1 || cell.y < y0 || cell.y > y1 {
                continue;
            }
            // Skip whitespace-only cells: blanks carry whatever style
            // the renderer left behind but no visible ink, so they
            // can't actually be "too bright".
            if cell.symbol.chars().all(|c| c.is_whitespace()) {
                continue;
            }
            let color = match channel {
                "fg" => cell.fg.as_deref(),
                "bg" => cell.bg.as_deref(),
                _ => unreachable!(),
            };
            let Some(color) = color else {
                continue;
            };
            let Some(rgb) = squeezy_tui::testing::cell_rgb(color) else {
                // Unknown name or indexed(...): we can't compute
                // luminance for it, so skip rather than falsely fail.
                continue;
            };
            let lum = squeezy_tui::testing::rgb_luminance(rgb);
            if lum > max {
                let worst_lum = worst.as_ref().map(|w| w.3).unwrap_or(0);
                if lum > worst_lum {
                    worst = Some((cell.x, cell.y, color.to_string(), lum, rgb));
                }
            }
        }
        match worst {
            None => "asserted_pass".into(),
            Some((x, y, color, lum, (r, g, b))) => format!(
                "asserted_fail: cell ({x},{y}) {channel}={color} (rgb {r},{g},{b}) luminance {lum} > {max}"
            ),
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
            cost_display: crate::frames::format_cost_micro_usd(0),
            ..Default::default()
        };
        let mut completed = false;
        let mut received_tool_call = false;
        let mut should_break_on_text = false;

        // Reset per-turn assistant text accumulator.
        self.last_assistant_text.lock().await.clear();
        // Phase 5: per-turn overlay tracker. Cleared so each captured
        // TuiFrame only carries the overlays that opened during *this*
        // turn.
        self.pending_overlays.lock().await.clear();

        // Phase 7: configurable per-event timeout with a
        // `ToolProgress`-aware reset. Default 60s (was 10s hardcoded);
        // any `ToolProgress` heartbeat received during the loop body
        // means the agent is still alive and the timeout resets on
        // the next iteration.
        let event_timeout =
            Duration::from_secs(self.scenario.expect.event_timeout_seconds.unwrap_or(60));

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
                    frame
                        .queued_tool_calls
                        .push(ToolCallSummary::from_call(&call.name, &call.arguments));
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
                    // Phase 2: also keep a typed (name, args) tuple so
                    // `Assertion::ToolCallWithArgs` can scan the full
                    // arg JSON instead of working off the truncated
                    // `args_preview` string.
                    self.observed_tool_calls
                        .lock()
                        .await
                        .push((call.name.clone(), call.arguments.clone()));
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
                    let details = approval_overlay_details(&request);
                    let preview = preview_overlay_lines(&request.preview);
                    self.pending_overlays
                        .lock()
                        .await
                        .push(crate::tui_capture::TuiOverlayEvent {
                            kind: "approval".into(),
                            summary: request.permission.summary.clone(),
                            disposition: recorded.clone(),
                            details,
                            preview,
                        });
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::Approval {
                            request: json!({
                                "tool": request.tool_name.clone(),
                                "summary": request.permission.summary.clone(),
                            }),
                            decision: recorded,
                        },
                    )?;
                    let _ = decision_tx.send(decision);
                }
                AgentEvent::McpElicitationRequested {
                    turn_id,
                    request,
                    response_tx,
                } => {
                    let turn_str = format!("{turn_id:?}");
                    let body = serde_json::to_value(&request).unwrap_or(Value::Null);
                    // Phase 2: consult the scenario action queue. A
                    // matching `RespondElicitation` consumes the slot
                    // and decides; absent any match we fall back to
                    // Cancel (mirrors the pre-Phase-2 behavior).
                    let (response, status) = self.decide_elicitation(&request).await;
                    self.pending_overlays
                        .lock()
                        .await
                        .push(crate::tui_capture::TuiOverlayEvent {
                            kind: "mcp_elicitation".into(),
                            summary: format!("server={}", request.server),
                            disposition: status.clone(),
                            details: vec![format!("server: {}", request.server)],
                            preview: Vec::new(),
                        });
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::ActionStep {
                            action: json!({
                                "kind": "mcp_elicitation",
                                "request": body,
                            }),
                            status,
                        },
                    )?;
                    match response {
                        Some(reply) => {
                            let _ = response_tx.send(reply);
                        }
                        None => drop(response_tx),
                    }
                }
                AgentEvent::RequestUserInputRequested {
                    turn_id,
                    request,
                    response_tx,
                } => {
                    let turn_str = format!("{turn_id:?}");
                    let body = serde_json::to_value(&request).unwrap_or(Value::Null);
                    let (response, status) = self.decide_user_input(&request).await;
                    self.pending_overlays
                        .lock()
                        .await
                        .push(crate::tui_capture::TuiOverlayEvent {
                            kind: "request_user_input".into(),
                            summary: request.question.clone(),
                            disposition: status.clone(),
                            details: user_input_overlay_details(&request),
                            preview: Vec::new(),
                        });
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::ActionStep {
                            action: json!({
                                "kind": "request_user_input",
                                "request": body,
                            }),
                            status,
                        },
                    )?;
                    let _ = response_tx.send(response);
                }
                AgentEvent::ContextCompacted { turn_id, report } => {
                    let turn_str = format!("{turn_id:?}");
                    // `ContextCompactionReport` carries `record` (a
                    // `ContextCompactionRecord`) + `summary` + `dropped`
                    // + `post_compact` Vecs. The record itself is
                    // Serialize via `squeezy-core`, so capturing the
                    // structured form gives findings rules a typed
                    // handle on before/after token totals and trigger
                    // reason. Fall back to debug if serialization ever
                    // fails.
                    let report_value = serde_json::to_value(&report.record)
                        .unwrap_or_else(|_| json!({"debug": format!("{:?}", report.record)}));
                    let value = json!({
                        "record": report_value,
                        "summary": report.summary,
                        "dropped_items": report.dropped.len(),
                    });
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::ContextCompacted { report: value },
                    )?;
                }
                AgentEvent::TaskStateUpdated { turn_id, snapshot } => {
                    let turn_str = format!("{turn_id:?}");
                    // v3: emit the full TaskStateSnapshot as structured
                    // JSON so rules can read `steps`, `blocker`,
                    // `next_action`, `verification`, `recent_changes`,
                    // `replan_reason` without re-parsing a debug
                    // string. `summary` and `status` still live at the
                    // same JSON path so the existing
                    // `task_snapshot_marks_help` helper keeps working.
                    let value = serde_json::to_value(&snapshot).unwrap_or_else(|_| {
                        json!({
                            "debug": format!("{:?}", snapshot),
                            "summary": snapshot.summary,
                            "status": snapshot.status.as_str(),
                        })
                    });
                    // Phase 2: keep the typed snapshot so
                    // `Assertion::TaskStateContains` can match on
                    // step titles / blockers without re-parsing JSON.
                    self.task_snapshots.lock().await.push(snapshot.clone());
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::TaskStateUpdated { snapshot: value },
                    )?;
                }
                AgentEvent::McpStatusUpdated { turn_id, snapshot } => {
                    let turn_str = format!("{turn_id:?}");
                    // v3 typed variant: serialize the per-server map
                    // (each McpServerStatus is Serialize) so rules can
                    // detect Failed / Disconnected entries.
                    let servers = serde_json::to_value(&snapshot.per_server).unwrap_or(Value::Null);
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::McpStatusUpdated {
                            servers,
                            generated_unix_millis: snapshot
                                .generated_unix_millis
                                .min(u64::MAX as u128)
                                as u64,
                        },
                    )?;
                }
                AgentEvent::JobUpdated { job } => {
                    // v3 typed variant: build a structured JSON view
                    // of the snapshot. JobSnapshot is not Serialize
                    // upstream, so construct fields explicitly to
                    // keep the schema diffable.
                    let progress = job.progress.as_ref().map(|p| {
                        json!({
                            "completed": p.completed,
                            "total": p.total,
                            "message": p.message,
                        })
                    });
                    let job_value = json!({
                        "id": job.id,
                        "kind": job.kind.as_str(),
                        "status": job.status.as_str(),
                        "title": job.title,
                        "progress": progress,
                        "result_summary": job.result_summary,
                        "output_handle": job.output_handle,
                        "turn_id": job.turn_id.map(|t| format!("{t:?}")),
                        "tool_name": job.tool_name,
                        "call_id": job.call_id,
                        "subagent_id": job.subagent_id,
                        "created_at_ms": job.created_at_ms,
                        "updated_at_ms": job.updated_at_ms,
                        "ended_at_ms": job.ended_at_ms,
                    });
                    self.capture
                        .record(None, EvalEventKind::JobUpdated { job: job_value })?;
                }
                AgentEvent::JobNotification { notification } => {
                    self.capture.record(
                        None,
                        EvalEventKind::JobNotification {
                            job_id: notification.job_id,
                            job_kind: notification.kind.as_str().to_string(),
                            status: notification.status.as_str().to_string(),
                            title: notification.title.clone(),
                            summary: notification.summary.clone(),
                            notification_ts_unix_ms: notification.ts_unix_ms,
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
                AgentEvent::SubagentRejected {
                    turn_id,
                    agent,
                    reason,
                    limit,
                    active,
                } => {
                    let turn_str = format!("{turn_id:?}");
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::SubagentEvent {
                            event: json!({
                                "kind": "rejected",
                                "agent": agent,
                                "reason": reason.as_str(),
                                "limit": limit,
                                "active": active,
                            }),
                        },
                    )?;
                }
                AgentEvent::AiReviewerTripped { turn_id, reason } => {
                    let turn_str = format!("{turn_id:?}");
                    self.capture
                        .record(Some(turn_str), EvalEventKind::AiReviewerTripped { reason })?;
                }
                AgentEvent::CostWarning { turn_id, status } => {
                    let turn_str = format!("{turn_id:?}");
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::CostWarning {
                            spent_usd_micros: status.spent_usd_micros,
                            cap_usd_micros: status.cap_usd_micros,
                            percent: status.percent,
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
                AgentEvent::ReasoningDelta { turn_id, delta } => {
                    let turn_str = format!("{turn_id:?}");
                    self.capture
                        .record(Some(turn_str), EvalEventKind::ReasoningDelta { delta })?;
                }
                AgentEvent::ReasoningSegment { turn_id, snapshot } => {
                    let turn_str = format!("{turn_id:?}");
                    let payload = serde_json::to_value(&snapshot.payload).unwrap_or(Value::Null);
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::ReasoningSegment {
                            display_text: snapshot.display_text,
                            payload,
                        },
                    )?;
                }
                AgentEvent::ShellSandboxBestEffortFallback {
                    turn_id,
                    backend,
                    fallback_count,
                } => {
                    let turn_str = format!("{turn_id:?}");
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::ShellSandboxDegraded {
                            backend,
                            fallback_count,
                        },
                    )?;
                }
                AgentEvent::Completed {
                    turn_id,
                    message,
                    response_id,
                    cost,
                    metrics,
                    context_estimate,
                    stop_reason,
                    reasoning_only_stop,
                } => {
                    let turn_str = format!("{turn_id:?}");
                    frame.elapsed_ms = turn_start.elapsed().as_millis() as u64;
                    frame.input_tokens = cost.input_tokens.unwrap_or(0);
                    frame.output_tokens = cost.output_tokens.unwrap_or(0);
                    frame.finish = FrameFinish::Completed;
                    frame.stop_reason = stop_reason.clone();
                    frame.reasoning_only_stop = reasoning_only_stop;
                    *self.last_stop_reason.lock().await = stop_reason.clone();
                    let cost_micro =
                        squeezy_llm::estimate_cost(self.provider_name, &self.model, &cost)
                            .unwrap_or(0);
                    frame.cost_micro_usd = cost_micro;
                    frame.cost_display = crate::frames::format_cost_micro_usd(cost_micro);
                    *self.total_input_tokens.lock().await += frame.input_tokens;
                    *self.total_cost_micro_usd.lock().await += cost_micro;
                    let metrics_v = serde_json::to_value(&metrics).unwrap_or(Value::Null);
                    let cost_v = serde_json::to_value(&cost).unwrap_or(Value::Null);
                    let message_v = serde_json::to_value(&message).ok();
                    let context_estimate_v = serde_json::to_value(&context_estimate).ok();
                    // Surface `dropped_tool_calls` if a future
                    // `TurnMetrics.dropped_tool_calls` field lands
                    // upstream. Read positionally so eval keeps
                    // working without a squeezy-core change; reads as
                    // 0 today. The `expect_dropped_tool_calls` rule
                    // (Phase 4) lights up automatically once the
                    // chat-completions provider wires its counter.
                    frame.dropped_tool_calls = metrics_v
                        .get("dropped_tool_calls")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    self.capture.record(
                        Some(turn_str),
                        EvalEventKind::TurnCompleted {
                            metrics: metrics_v,
                            cost: cost_v,
                            stop_reason: stop_reason.clone(),
                            reasoning_only_stop,
                            message: message_v,
                            response_id: response_id.clone(),
                            context_estimate: context_estimate_v,
                        },
                    )?;
                    completed = true;
                    break;
                }
                AgentEvent::Cancelled {
                    turn_id,
                    cost,
                    metrics: _,
                } => {
                    let turn_str = format!("{turn_id:?}");
                    frame.elapsed_ms = turn_start.elapsed().as_millis() as u64;
                    // Mirror the `Completed` arm's accounting on the
                    // cancel path. A mid-stream cancel still bills for
                    // whatever input the provider already saw and
                    // whatever output it already streamed back; reporting
                    // those as zero would silently under-count the run's
                    // `totals` in `run.json`. The cost broker on the
                    // agent side seeds `cost.estimated_usd_micros` from
                    // the pricing registry when the provider does not
                    // emit a usage payload, so we re-apply the same
                    // fallback here in case the cancel surfaced through a
                    // path that left the field unfilled.
                    frame.input_tokens = cost.input_tokens.unwrap_or(0);
                    frame.output_tokens = cost.output_tokens.unwrap_or(0);
                    let cost_micro = cost.estimated_usd_micros.unwrap_or_else(|| {
                        squeezy_llm::estimate_cost(self.provider_name, &self.model, &cost)
                            .unwrap_or(0)
                    });
                    frame.cost_micro_usd = cost_micro;
                    frame.cost_display = crate::frames::format_cost_micro_usd(cost_micro);
                    *self.total_input_tokens.lock().await += frame.input_tokens;
                    *self.total_cost_micro_usd.lock().await += cost_micro;
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
        // Phase 5: optional per-turn TUI capture into frames_tui.jsonl.
        // Skipped unless `[tui_capture] enabled = true` is set on the
        // scenario.
        if let Some(writer) = self.tui_capture.as_ref() {
            let overlays = self.pending_overlays.lock().await.clone();
            let rendered = crate::tui_capture::render_capture_to_grid(
                &frame.assistant_text,
                &overlays,
                writer.width(),
                writer.height(),
            )?;
            let tui_frame = crate::tui_capture::TuiFrame {
                turn_id: frame.turn_id.clone(),
                width: writer.width(),
                height: writer.height(),
                cells: rendered.cells,
                plain_text: rendered.plain_text,
                ansi: rendered.ansi,
                visual_truncated: rendered.visual_truncated,
                omitted_line_count: rendered.omitted_line_count,
                overlays,
                trigger: Some(crate::tui_capture::TuiFrameTrigger {
                    kind: "turn_completed".into(),
                    step_index: None,
                    key: None,
                }),
                transcript: Vec::new(),
                status_text: None,
            };
            writer.write(&tui_frame)?;
        }
        *self.wall_clock_seconds.lock().await = self.run_start.elapsed().as_secs();
        Ok(())
    }

    /// Apply a unified diff to a single workspace file via `git apply`.
    /// We shell out instead of writing our own patch applier so multi-hunk,
    /// context-fuzzy, and rename-style diffs behave identically to what a
    /// human would expect — and so the failure surface (`error: corrupt
    /// patch ...`) is the same one a developer recognizes from CI.
    fn apply_unified_diff(&self, path: &Path, unified_diff: &str) -> Result<String, EvalError> {
        use std::io::Write as _;
        let workspace_root = self.agent.as_ref().workspace_root_clone();
        // Materialize the diff to disk so `git apply -` is not at the
        // mercy of stdin lifetimes on slow CI; the tempfile lives only
        // for the duration of the apply.
        let mut tmp =
            workspace_root.join(format!(".squeezy-eval-apply-{}.patch", std::process::id()));
        // Avoid collisions when multiple parallel applies fire.
        let mut n: u32 = 0;
        while tmp.exists() {
            n = n.wrapping_add(1);
            tmp = workspace_root.join(format!(
                ".squeezy-eval-apply-{}-{}.patch",
                std::process::id(),
                n
            ));
        }
        {
            let mut f = std::fs::File::create(&tmp)
                .map_err(|err| EvalError::Io(format!("create {tmp:?}: {err}")))?;
            f.write_all(unified_diff.as_bytes())
                .map_err(|err| EvalError::Io(format!("write {tmp:?}: {err}")))?;
        }
        let output = std::process::Command::new("git")
            .current_dir(&workspace_root)
            .args([
                "apply",
                "--whitespace=nowarn",
                tmp.to_string_lossy().as_ref(),
            ])
            .output()
            .map_err(|err| EvalError::Workspace(format!("spawn git apply: {err}")))?;
        let _ = std::fs::remove_file(&tmp);
        if output.status.success() {
            Ok(format!("applied_diff:{}", path.display()))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Ok(format!(
                "asserted_fail: git apply rejected {}: {}",
                path.display(),
                stderr.trim().chars().take(200).collect::<String>()
            ))
        }
    }

    /// Look for a queued `RespondElicitation` that matches `request`,
    /// consume it, and produce the response payload + status string.
    /// Falls back to `Cancel` (mirrors pre-Phase-2 behavior) so a
    /// missing scripted response never hangs the agent.
    async fn decide_elicitation(
        &self,
        request: &squeezy_tools::McpElicitationRequest,
    ) -> (Option<squeezy_tools::McpElicitationResponse>, String) {
        let mut queue = self.action_queue.lock().await;
        let mut found_index: Option<usize> = None;
        for (idx, action) in queue.iter().enumerate() {
            if elicitation_matches(action, request) {
                found_index = Some(idx);
                break;
            }
        }
        if let Some(idx) = found_index {
            let action = queue.remove(idx);
            drop(queue);
            if let Action::RespondElicitation { decision, .. } = action {
                let (resp, status) = match decision {
                    crate::scenario::ElicitationDecision::Accept { content } => (
                        squeezy_tools::McpElicitationResponse::accept(content),
                        "accepted".to_string(),
                    ),
                    crate::scenario::ElicitationDecision::Decline => (
                        squeezy_tools::McpElicitationResponse::decline(),
                        "declined".into(),
                    ),
                    crate::scenario::ElicitationDecision::Cancel => (
                        squeezy_tools::McpElicitationResponse::cancel(),
                        "cancelled".into(),
                    ),
                };
                return (Some(resp), status);
            }
        }
        // No scripted response — fall back to auto-cancel and flag the
        // status so triage can spot the gap.
        (None, "auto_cancelled".into())
    }

    /// Same shape as `decide_elicitation` but for `RequestUserInput`.
    async fn decide_user_input(
        &self,
        request: &squeezy_agent::RequestUserInputRequest,
    ) -> (squeezy_agent::RequestUserInputResponse, String) {
        let mut queue = self.action_queue.lock().await;
        let mut found_index: Option<usize> = None;
        for (idx, action) in queue.iter().enumerate() {
            if user_input_matches(action, request) {
                found_index = Some(idx);
                break;
            }
        }
        if let Some(idx) = found_index {
            let action = queue.remove(idx);
            drop(queue);
            if let Action::RespondUserInput { decision, .. } = action {
                return match decision {
                    crate::scenario::UserInputDecision::Choice { value } => (
                        squeezy_agent::RequestUserInputResponse::choice(value.clone()),
                        format!("choice:{value}"),
                    ),
                    crate::scenario::UserInputDecision::Freeform { text } => (
                        squeezy_agent::RequestUserInputResponse::freeform(text.clone()),
                        format!("freeform:{text}"),
                    ),
                    crate::scenario::UserInputDecision::Cancel => (
                        squeezy_agent::RequestUserInputResponse::cancelled(),
                        "cancelled".into(),
                    ),
                };
            }
        }
        (
            squeezy_agent::RequestUserInputResponse::cancelled(),
            "auto_cancelled".into(),
        )
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

fn elicitation_matches(action: &Action, request: &squeezy_tools::McpElicitationRequest) -> bool {
    let Action::RespondElicitation { r#match, .. } = action else {
        return false;
    };
    let Some(m) = r#match.as_ref() else {
        // Empty match → fire on the first elicitation we see.
        return true;
    };
    if let Some(server) = &m.server
        && server != &request.server
    {
        return false;
    }
    if let Some(kind) = &m.kind {
        let actual = match request.kind {
            squeezy_tools::McpElicitationKind::Form => "form",
            squeezy_tools::McpElicitationKind::Url => "url",
        };
        if !kind.eq_ignore_ascii_case(actual) {
            return false;
        }
    }
    true
}

fn user_input_matches(action: &Action, request: &squeezy_agent::RequestUserInputRequest) -> bool {
    let Action::RespondUserInput { r#match, .. } = action else {
        return false;
    };
    let Some(m) = r#match.as_ref() else {
        return true;
    };
    if let Some(needle) = &m.prompt_contains
        && !request.question.contains(needle)
    {
        return false;
    }
    true
}

fn approval_overlay_details(request: &squeezy_agent::ToolApprovalRequest) -> Vec<String> {
    let mut details = vec![
        format!("tool: {}", request.tool_name),
        format!("target: {}", request.permission.target),
        format!("risk: {:?}", request.permission.risk),
        format!("reason: {}", request.reason),
    ];
    if let Some(context) = &request.context
        && !context.trim().is_empty()
    {
        details.push(format!("context: {}", normalize_overlay_text(context)));
    }
    for (key, value) in request.permission.metadata.iter().take(4) {
        details.push(format!("{key}: {}", normalize_overlay_text(value)));
    }
    details
}

fn user_input_overlay_details(request: &squeezy_agent::RequestUserInputRequest) -> Vec<String> {
    let mut details = vec![format!("question: {}", request.question)];
    if !request.choices.is_empty() {
        details.push(format!(
            "choices: {}",
            request
                .choices
                .iter()
                .map(|choice| format!("{}={}", choice.label, choice.value))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    details.push(format!("freeform: {}", request.allow_freeform));
    details
}

fn preview_overlay_lines(lines: &[squeezy_tools::preview::PreviewLine]) -> Vec<String> {
    lines
        .iter()
        .map(|line| match line {
            squeezy_tools::preview::PreviewLine::Plain { text } => text.clone(),
            squeezy_tools::preview::PreviewLine::Diff { added, line } => {
                format!("{}{}", if *added { "+" } else { "-" }, line)
            }
            squeezy_tools::preview::PreviewLine::Highlighted { lang, text } => {
                format!("{lang}: {text}")
            }
            squeezy_tools::preview::PreviewLine::Warning { text } => format!("warning: {text}"),
        })
        .map(|text| normalize_overlay_text(&text))
        .collect()
}

fn normalize_overlay_text(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(collapsed.len());
    let mut chars = collapsed.chars().peekable();
    while let Some(ch) = chars.next() {
        out.push(ch);
        if matches!(ch, '.' | '!' | '?')
            && let Some(next) = chars.peek()
            && next.is_ascii_uppercase()
        {
            out.push(' ');
        }
    }
    out
}

fn action_to_value(action: &Action) -> Value {
    serde_json::to_value(action).unwrap_or(Value::Null)
}

/// Short label for an `Action` used in trace step-boundary records.
/// Lives next to the dispatch helpers so it stays in sync with the
/// `Action` enum without exposing the full serialized shape.
fn action_kind_label(action: &Action) -> &'static str {
    match action {
        Action::Approve { .. } => "approve",
        Action::Deny { .. } => "deny",
        Action::SlashCommand { .. } => "slash_command",
        Action::EditFile { .. } => "edit_file",
        Action::WaitSeconds { .. } => "wait_seconds",
        Action::CancelTurn { .. } => "cancel_turn",
        Action::Assert { .. } => "assert",
        Action::InjectUserText { .. } => "inject_user_text",
        Action::RespondElicitation { .. } => "respond_elicitation",
        Action::InjectMcpElicitation { .. } => "inject_mcp_elicitation",
        Action::RespondUserInput { .. } => "respond_user_input",
        Action::ApplyDiff { .. } => "apply_diff",
        Action::SwitchMode { .. } => "switch_mode",
        Action::AttachFile { .. } => "attach_file",
        Action::DetachAttachment { .. } => "detach_attachment",
        Action::SendKey { .. } => "send_key",
        Action::SendKeys { .. } => "send_keys",
        Action::CaptureSessionId { .. } => "capture_session_id",
    }
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

/// Internal extension: lift the agent's workspace_root off its
/// AppConfig. Scenarios with `[workspace] snapshot = true` build the
/// agent against a snapshot worktree; resolving relative edit_file
/// paths via `env::current_dir()` (the previous fallback) wrote into
/// the host repo instead. squeezy-nyg8.1.
trait AgentExt {
    fn workspace_root_clone(&self) -> PathBuf;
}

impl AgentExt for Agent {
    fn workspace_root_clone(&self) -> PathBuf {
        self.config().workspace_root.clone()
    }
}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod tests;
