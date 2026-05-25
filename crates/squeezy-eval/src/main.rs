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
        } => run_cmd(scenario, workspace_override, no_triage, emit, gh_repo, out).await,
        Command::List { dir } => list_cmd(dir),
        Command::Replay { trace } => replay_cmd(trace),
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
) -> Result<(), squeezy_eval::driver::EvalError> {
    let mut scenario = squeezy_eval::scenario::load(&scenario_path)?;
    if let Some(path) = workspace_override {
        scenario.workspace = squeezy_eval::scenario::WorkspaceSpec::Local { path };
    }
    let options = squeezy_eval::RunOptions {
        scenario_path: scenario_path.clone(),
        out_root: out,
        run_triage: !no_triage,
        emit_github: emit.as_deref() == Some("github"),
        gh_repo,
    };
    let outcome = squeezy_eval::run_scenario(scenario, options).await?;
    println!("eval run complete: {}", outcome.run_dir.display());
    println!(
        "  trace: {} events  frames: {}  tickets: {}",
        outcome.trace_event_count, outcome.frame_count, outcome.ticket_count
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
