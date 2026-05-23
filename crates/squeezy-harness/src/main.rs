use std::path::PathBuf;

use clap::{Parser, Subcommand};
use squeezy_harness::{
    HarnessConfig, RunnerKind, default_runners, default_tasks_dir, load_tasks, run_harness,
    summarize,
};

#[derive(Debug, Parser)]
#[command(
    name = "squeezy-harness",
    version,
    about = "Run Squeezy validation tasks"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    List {
        #[arg(long)]
        tasks: Option<PathBuf>,
    },
    Run {
        #[arg(long)]
        tasks: Option<PathBuf>,
        #[arg(long = "runner")]
        runners: Vec<RunnerKind>,
        #[arg(long)]
        jsonl: Option<PathBuf>,
        #[arg(long)]
        trace_dir: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> squeezy_core::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::List { tasks } => {
            let tasks_dir = tasks.unwrap_or_else(default_tasks_dir);
            for task in load_tasks(&tasks_dir)? {
                println!("{}\t{}", task.id, task.title);
            }
        }
        Command::Run {
            tasks,
            runners,
            jsonl,
            trace_dir,
        } => {
            let runners = if runners.is_empty() {
                default_runners()
            } else {
                runners
            };
            let results = run_harness(HarnessConfig {
                tasks_dir: tasks.unwrap_or_else(default_tasks_dir),
                runners,
                jsonl_path: jsonl,
                trace_dir,
            })
            .await?;
            let summary = serde_json::to_string_pretty(&summarize(&results)).map_err(|err| {
                squeezy_core::SqueezyError::Agent(format!("failed to serialize summary: {err}"))
            })?;
            println!("{summary}");
            if results
                .iter()
                .any(|result| !matches!(result.status, squeezy_harness::TaskStatus::Passed))
            {
                return Err(squeezy_core::SqueezyError::Agent(
                    "one or more harness tasks failed".to_string(),
                ));
            }
        }
    }
    Ok(())
}
