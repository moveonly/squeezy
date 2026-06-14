use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

/// Worker-thread stack size for the eval runtime. Mirrors squeezy-cli's
/// `WORKER_THREAD_STACK_SIZE`. The agent code this harness drives can recurse
/// deeply on a tokio worker — e.g. the `grep` tool walks the workspace with
/// `ignore::Walk`, which compiles the repo's `.gitignore` globs into a single
/// regex (globset → regex-automata's recursive NFA compiler). On a large
/// `local = "."` workspace that compile overflows tokio's default 2 MiB worker
/// stack and SIGABRTs. The real CLI bumps its workers to 16 MiB for exactly
/// this reason; the eval runtime must match or scenarios crash here despite the
/// product being fine in production.
const WORKER_THREAD_STACK_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "squeezy-eval",
    version,
    about = "Agent-driven QA harness for Squeezy"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a scenario file against squeezy and write a trace + tickets bundle.
    Run {
        /// Path to the scenario TOML file.
        scenario: PathBuf,
        /// Override the scenario's workspace with a local path.
        #[arg(long)]
        workspace_override: Option<PathBuf>,
        /// Skip the LLM triage pass even if the scenario enables it.
        #[arg(long)]
        no_triage: bool,
        /// Optionally open tickets in the listed sink (currently only "github").
        #[arg(long)]
        emit: Option<String>,
        /// GitHub repo for `--emit github` (e.g. owner/name).
        #[arg(long)]
        gh_repo: Option<String>,
        /// Output root directory; defaults to `target/eval`.
        #[arg(long, default_value = "target/eval")]
        out: PathBuf,
        /// Suppress live streaming output (recommended for CI). Default
        /// is to stream squeezy's activity to stdout so a watching user
        /// sees what the agent is doing.
        #[arg(long)]
        quiet: bool,
    },
    /// List bundled or directory-provided scenarios.
    List {
        /// Directory to scan; defaults to the bundled `fixtures/scenarios/`.
        dir: Option<PathBuf>,
    },
    /// Print a one-line summary of a recorded eval trace.
    Replay {
        /// Path to a `trace.jsonl` produced by a previous run.
        trace: PathBuf,
    },
    /// Render a run directory as a chronological markdown transcript.
    View {
        /// Run directory containing `trace.jsonl` + `frames.jsonl` + `run.json`.
        run: PathBuf,
    },
    /// Compare two run directories and print a markdown or JSON delta.
    Diff {
        /// First run directory (the baseline).
        a: PathBuf,
        /// Second run directory (the candidate).
        b: PathBuf,
        /// Output format: `markdown` (default) or `json`.
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Refuse to diff when the two runs have different trace
        /// `schema_version` (v2 vs v3+). Defaults to off for
        /// backwards compatibility — old `diff` callers keep
        /// working.
        #[arg(long)]
        schema_check: bool,
    },
    /// Run every scenario in a directory and exit non-zero if any
    /// scenario violates the `--fail-on` policy.
    Check {
        /// Directory containing scenario TOMLs.
        dir: PathBuf,
        /// Optional JUnit XML output path for CI consumers.
        #[arg(long)]
        junit: Option<PathBuf>,
        /// Comma-separated policy: any of `findings`, `expectations`,
        /// `errors`, `input-regression`. Default: `expectations,errors`.
        /// `input-regression` is opt-in and only bites when a baseline is
        /// provided via `--input-baseline`.
        #[arg(long, default_value = "expectations,errors")]
        fail_on: String,
        /// Output root directory; defaults to `target/eval`.
        #[arg(long, default_value = "target/eval")]
        out: PathBuf,
        /// Max scenarios to run concurrently. Defaults to 1
        /// (serial); use a higher number for fan-out CI runs.
        #[arg(long)]
        parallelism: Option<usize>,
        /// Optional JSON file storing per-scenario input-token baselines.
        /// When set, each run is compared against its baseline; when absent,
        /// the input-token regression gate is inert.
        #[arg(long)]
        input_baseline: Option<PathBuf>,
        /// Fraction a run may exceed its baseline input tokens before the gate
        /// flags it (e.g. 0.10 == 10%). Defaults to 0.10.
        #[arg(long)]
        input_tolerance: Option<f64>,
        /// Record newly-seen scenarios into the baseline file (and only those;
        /// existing baselines are never overwritten). Off by default so a CI
        /// run can't silently move the goalposts.
        #[arg(long)]
        update_baseline: bool,
        /// Run only scenarios that need no provider key or external setup
        /// (`provider = "mock"` and `hermetic = true`). Skipped scenarios are
        /// logged. Used by the offline CI job.
        #[arg(long)]
        hermetic: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Build the runtime explicitly so we can bound shutdown with
    // `shutdown_timeout`. The default `#[tokio::main]` drops the
    // runtime, which blocks until every spawned task completes —
    // including the telemetry crate's 5-second
    // `tokio::spawn(time::sleep(FLUSH_INTERVAL))` and any
    // fire-and-forget MCP/refresh task that outlived its parent.
    // Parallel `check` workers were stalling for ~5 minutes after
    // `run.json` was already on disk because of this, prompting the
    // sweep watchdog to SIGKILL them mid-flush.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_THREAD_STACK_SIZE)
        .build()
        .expect("build tokio runtime");
    let result = runtime.block_on(async {
        match cli.command {
            Command::Run {
                scenario,
                workspace_override,
                no_triage,
                emit,
                gh_repo,
                out,
                quiet,
            } => {
                run_cmd(
                    scenario,
                    workspace_override,
                    no_triage,
                    emit,
                    gh_repo,
                    out,
                    quiet,
                )
                .await
            }
            Command::List { dir } => list_cmd(dir),
            Command::Replay { trace } => replay_cmd(trace),
            Command::View { run } => view_cmd(run),
            Command::Diff {
                a,
                b,
                format,
                schema_check,
            } => diff_cmd(a, b, format, schema_check),
            Command::Check {
                dir,
                junit,
                fail_on,
                out,
                parallelism,
                input_baseline,
                input_tolerance,
                update_baseline,
                hermetic,
            } => {
                check_cmd(
                    dir,
                    junit,
                    fail_on,
                    out,
                    parallelism,
                    input_baseline,
                    input_tolerance,
                    update_baseline,
                    hermetic,
                )
                .await
            }
        }
    });
    // 2 s is well beyond the wall-clock of any tracked task the
    // post-run drain leaves behind (telemetry's `FLUSH_INTERVAL` is
    // 5 s but its `send_batch` is bounded by `REQUEST_TIMEOUT = 2 s`)
    // and well short of the 5-minute sweep watchdog window.
    runtime.shutdown_timeout(Duration::from_secs(2));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("squeezy-eval: {err:#}");
            ExitCode::from(1)
        }
    }
}

async fn run_cmd(
    scenario_path: PathBuf,
    workspace_override: Option<PathBuf>,
    no_triage: bool,
    emit: Option<String>,
    gh_repo: Option<String>,
    out: PathBuf,
    quiet: bool,
) -> Result<(), squeezy_eval::driver::EvalError> {
    let mut scenario = squeezy_eval::scenario::load(&scenario_path)?;
    if let Some(path) = workspace_override {
        scenario.workspace = squeezy_eval::scenario::WorkspaceSpec::Local {
            path,
            snapshot: false,
            snapshot_ref: None,
        };
    }
    let options = squeezy_eval::RunOptions {
        scenario_path: scenario_path.clone(),
        out_root: out,
        run_triage: !no_triage,
        emit_github: emit.as_deref() == Some("github"),
        gh_repo,
        live: !quiet,
    };
    let outcome = squeezy_eval::run_scenario(scenario, options).await?;
    println!("eval run complete: {}", outcome.run_dir.display());
    println!(
        "  trace: {} events  frames: {}  tickets: {}  cost: {}",
        outcome.trace_event_count,
        outcome.frame_count,
        outcome.ticket_count,
        squeezy_eval::frames::format_cost_micro_usd(outcome.cost_micro_usd),
    );
    Ok(())
}

fn list_cmd(dir: Option<PathBuf>) -> Result<(), squeezy_eval::driver::EvalError> {
    let dir = dir.unwrap_or_else(|| PathBuf::from("crates/squeezy-eval/fixtures/scenarios"));
    let mut paths = squeezy_eval::ci::collect_scenario_paths(&dir)?;
    paths.sort();
    for path in paths {
        match squeezy_eval::scenario::load(&path) {
            Ok(scenario) => {
                println!("{:<40} {}", scenario.id, scenario.title);
                println!("  path: {}", path.display());
            }
            Err(err) => {
                println!("{:<40} (parse error: {err})", path.display().to_string());
            }
        }
    }
    Ok(())
}

fn replay_cmd(trace: PathBuf) -> Result<(), squeezy_eval::driver::EvalError> {
    let summary = squeezy_eval::capture::summarize_trace(&trace)?;
    println!("trace: {}", trace.display());
    println!("  events:        {}", summary.event_count);
    println!("  turns:         {}", summary.turn_count);
    println!("  tool_calls:    {}", summary.tool_call_count);
    println!("  tool_errors:   {}", summary.tool_error_count);
    println!("  wall_clock_ms: {}", summary.wall_clock_ms);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn check_cmd(
    dir: PathBuf,
    junit: Option<PathBuf>,
    fail_on: String,
    out: PathBuf,
    parallelism: Option<usize>,
    input_baseline: Option<PathBuf>,
    input_tolerance: Option<f64>,
    update_baseline: bool,
    hermetic: bool,
) -> Result<(), squeezy_eval::driver::EvalError> {
    let opts = squeezy_eval::ci::CheckOptions {
        dir,
        out_root: out,
        fail_on: squeezy_eval::ci::FailOn::parse(&fail_on),
        junit_path: junit,
        parallelism,
        input_regression: squeezy_eval::ci::InputRegression {
            baseline_path: input_baseline,
            tolerance: input_tolerance.unwrap_or(squeezy_eval::ci::DEFAULT_INPUT_TOLERANCE),
            update_baseline,
        },
        hermetic_only: hermetic,
        emit_progress: true,
    };
    let report = squeezy_eval::ci::run_check(opts).await?;
    let total = report.results.len();
    let failed = report.results.iter().filter(|r| !r.passed).count();
    for r in &report.results {
        let status = if r.passed { "PASS" } else { "FAIL" };
        println!("{status}  {:<40}  {}ms", r.name, r.elapsed_ms);
        if let Some(err) = &r.error {
            println!("       error: {err}");
        }
        if !r.expectation_rule_ids.is_empty() {
            println!("       expectations: {:?}", r.expectation_rule_ids);
        }
        if !r.finding_rule_ids.is_empty() {
            println!("       findings:     {:?}", r.finding_rule_ids);
        }
        if let Some(verdict) = &r.input_regression {
            println!("       {}", verdict.message());
        }
    }
    println!("\nsummary: {failed}/{total} failed");
    if report.passed() {
        Ok(())
    } else {
        Err(squeezy_eval::driver::EvalError::Internal(format!(
            "{failed} scenario(s) failed the fail-on policy"
        )))
    }
}

fn diff_cmd(
    a: PathBuf,
    b: PathBuf,
    format: String,
    schema_check: bool,
) -> Result<(), squeezy_eval::driver::EvalError> {
    let fmt = squeezy_eval::diff::DiffFormat::parse(&format);
    let report = squeezy_eval::diff::diff_runs_with_schema_check(&a, &b, fmt, schema_check)?;
    print!("{report}");
    Ok(())
}

fn view_cmd(run: PathBuf) -> Result<(), squeezy_eval::driver::EvalError> {
    let rendered = squeezy_eval::view::render(&run)?;
    print!("{rendered}");
    Ok(())
}
