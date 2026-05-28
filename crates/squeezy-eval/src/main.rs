use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
        /// Comma-separated policy: any of `findings`, `expectations`, `errors`.
        /// Default: `expectations,errors`.
        #[arg(long, default_value = "expectations,errors")]
        fail_on: String,
        /// Output root directory; defaults to `target/eval`.
        #[arg(long, default_value = "target/eval")]
        out: PathBuf,
        /// Max scenarios to run concurrently. Defaults to 1
        /// (serial); use a higher number for fan-out CI runs.
        #[arg(long)]
        parallelism: Option<usize>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
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
        } => check_cmd(dir, junit, fail_on, out, parallelism).await,
    };
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
    let mut entries = std::fs::read_dir(&dir)
        .map_err(|err| squeezy_eval::driver::EvalError::Io(format!("read_dir {dir:?}: {err}")))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("toml"))
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
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

async fn check_cmd(
    dir: PathBuf,
    junit: Option<PathBuf>,
    fail_on: String,
    out: PathBuf,
    parallelism: Option<usize>,
) -> Result<(), squeezy_eval::driver::EvalError> {
    let opts = squeezy_eval::ci::CheckOptions {
        dir,
        out_root: out,
        fail_on: squeezy_eval::ci::FailOn::parse(&fail_on),
        junit_path: junit,
        parallelism,
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
