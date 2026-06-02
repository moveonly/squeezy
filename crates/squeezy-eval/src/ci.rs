//! `squeezy-eval check` — CI entry point.
//!
//! Iterates every `*.toml` scenario in a directory, runs each with
//! `--no-triage`, and exits non-zero when any scenario violates the
//! requested `fail_on` policy.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::WalkBuilder;
use tokio::sync::Semaphore;

use crate::driver::{EvalError, RunOptions, run_scenario};
use crate::scenario;

/// Recursively gather every `*.toml` scenario beneath `dir`. Used by
/// both `squeezy-eval list` and `squeezy-eval check` so that grouping
/// scenarios into subdirectories (e.g. `benchmarks/{natural,targeted}`)
/// does not require touching every call site.
pub fn collect_scenario_paths(dir: &Path) -> Result<Vec<PathBuf>, EvalError> {
    let walker = WalkBuilder::new(dir)
        .standard_filters(false)
        .hidden(false)
        .build();
    let mut out = Vec::new();
    for entry in walker {
        let entry = entry.map_err(|err| EvalError::Io(format!("walk {dir:?}: {err}")))?;
        let path = entry.path();
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let is_toml = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("toml"))
            .unwrap_or(false);
        if is_toml {
            out.push(path.to_path_buf());
        }
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct CheckOptions {
    pub dir: PathBuf,
    pub out_root: PathBuf,
    pub fail_on: FailOn,
    pub junit_path: Option<PathBuf>,
    /// Phase 7: max concurrent scenarios. Defaults to 1 (serial) for
    /// back-compat. Workspaces are already per-run-isolated, so
    /// higher values are safe modulo the process-wide env mutation
    /// in `Driver::run_scenario` (which a parallel runner needs to
    /// either accept or split into separate processes).
    pub parallelism: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct FailOn {
    pub findings: bool,
    pub expectations: bool,
    pub errors: bool,
}

impl FailOn {
    pub fn parse(spec: &str) -> Self {
        let mut fail_on = Self::default();
        for part in spec.split(',').map(str::trim) {
            if part.eq_ignore_ascii_case("findings") {
                fail_on.findings = true;
            } else if part.eq_ignore_ascii_case("expectations") {
                // Expectations are a subset of auto-findings (rules whose
                // ids start with `expect_`); treat the keyword as opt-in
                // separately from the broader `findings` bucket.
                fail_on.expectations = true;
            } else if part.eq_ignore_ascii_case("errors") {
                fail_on.errors = true;
            }
        }
        fail_on
    }
}

impl Default for CheckOptions {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("crates/squeezy-eval/fixtures/scenarios"),
            out_root: PathBuf::from("target/eval"),
            fail_on: FailOn {
                findings: false,
                expectations: true,
                errors: true,
            },
            junit_path: None,
            parallelism: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScenarioResult {
    pub name: String,
    pub path: PathBuf,
    pub passed: bool,
    pub error: Option<String>,
    pub finding_rule_ids: Vec<String>,
    pub expectation_rule_ids: Vec<String>,
    pub elapsed_ms: u128,
}

#[derive(Debug, Default, Clone)]
pub struct CheckReport {
    pub results: Vec<ScenarioResult>,
}

impl CheckReport {
    pub fn passed(&self) -> bool {
        self.results.iter().all(|r| r.passed)
    }
}

pub async fn run_check(opts: CheckOptions) -> Result<CheckReport, EvalError> {
    let mut entries = collect_scenario_paths(&opts.dir)?;
    entries.sort();

    // Phase 7: parallel runner. `parallelism = None | Some(1)` means
    // serial (back-compat); higher values run scenarios concurrently
    // behind a tokio Semaphore. Workspaces are isolated per run, but
    // env-var mutations in `run_scenario` are process-wide and can
    // race; callers who set `[env_vars]` on multiple scenarios should
    // either keep parallelism at 1 or ensure the keys don't collide.
    let parallelism = opts.parallelism.unwrap_or(1).max(1);
    let fail_on_findings = opts.fail_on.findings;
    let fail_on_expectations = opts.fail_on.expectations;
    let fail_on_errors = opts.fail_on.errors;
    let out_root = opts.out_root.clone();
    let semaphore = Arc::new(Semaphore::new(parallelism));

    let mut handles = Vec::with_capacity(entries.len());
    for path in entries {
        let permit_owner = semaphore.clone();
        let path_clone = path.clone();
        let out_root = out_root.clone();
        let handle = tokio::spawn(async move {
            let _permit = permit_owner
                .acquire_owned()
                .await
                .expect("semaphore closed");
            let started = std::time::Instant::now();
            let scenario = match scenario::load(&path_clone) {
                Ok(s) => s,
                Err(err) => {
                    return ScenarioResult {
                        name: path_clone.display().to_string(),
                        path: path_clone,
                        passed: !fail_on_errors,
                        error: Some(format!("{err}")),
                        finding_rule_ids: vec![],
                        expectation_rule_ids: vec![],
                        elapsed_ms: started.elapsed().as_millis(),
                    };
                }
            };
            let name = scenario.id.clone();
            let run_options = RunOptions {
                scenario_path: path_clone.clone(),
                out_root,
                run_triage: false,
                emit_github: false,
                gh_repo: None,
                live: false,
            };
            match run_scenario(scenario, run_options).await {
                Ok(outcome) => {
                    let (expectations, others): (Vec<_>, Vec<_>) = outcome
                        .findings
                        .iter()
                        .partition(|s| s.starts_with("[expect_"));
                    let expectation_rule_ids = rule_ids(&expectations);
                    let finding_rule_ids = rule_ids(&others);
                    let mut passed = true;
                    if fail_on_expectations && !expectation_rule_ids.is_empty() {
                        passed = false;
                    }
                    if fail_on_findings && !finding_rule_ids.is_empty() {
                        passed = false;
                    }
                    ScenarioResult {
                        name,
                        path: path_clone,
                        passed,
                        error: None,
                        finding_rule_ids,
                        expectation_rule_ids,
                        elapsed_ms: started.elapsed().as_millis(),
                    }
                }
                Err(err) => ScenarioResult {
                    name,
                    path: path_clone,
                    passed: !fail_on_errors,
                    error: Some(format!("{err}")),
                    finding_rule_ids: vec![],
                    expectation_rule_ids: vec![],
                    elapsed_ms: started.elapsed().as_millis(),
                },
            }
        });
        handles.push(handle);
    }

    let mut report = CheckReport::default();
    for handle in handles {
        match handle.await {
            Ok(result) => report.results.push(result),
            Err(err) => {
                report.results.push(ScenarioResult {
                    name: "<panicked>".into(),
                    path: PathBuf::new(),
                    passed: !opts.fail_on.errors,
                    error: Some(format!("scenario task panicked: {err}")),
                    finding_rule_ids: vec![],
                    expectation_rule_ids: vec![],
                    elapsed_ms: 0,
                });
            }
        }
    }
    // Restore deterministic ordering (parallel tasks can finish out of order).
    report.results.sort_by(|a, b| a.path.cmp(&b.path));

    if let Some(junit_path) = &opts.junit_path {
        write_junit(junit_path, &report)?;
    }
    Ok(report)
}

fn rule_ids<S: AsRef<str>>(items: &[S]) -> Vec<String> {
    items
        .iter()
        .filter_map(|s| {
            let s = s.as_ref();
            // Format produced by Driver: `[rule_id] summary`.
            let stripped = s.strip_prefix('[')?;
            let end = stripped.find(']')?;
            Some(stripped[..end].to_string())
        })
        .collect()
}

fn write_junit(path: &Path, report: &CheckReport) -> Result<(), EvalError> {
    use std::fmt::Write as _;
    let mut xml = String::new();
    let total = report.results.len();
    let failures = report.results.iter().filter(|r| !r.passed).count();
    let _ = writeln!(xml, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
    let _ = writeln!(
        xml,
        "<testsuite name=\"squeezy-eval\" tests=\"{total}\" failures=\"{failures}\">"
    );
    for r in &report.results {
        let time_seconds = (r.elapsed_ms as f64) / 1000.0;
        let _ = writeln!(
            xml,
            "  <testcase name=\"{}\" time=\"{:.3}\">",
            escape_xml(&r.name),
            time_seconds
        );
        if !r.passed {
            let detail = r.error.clone().unwrap_or_else(|| {
                format!(
                    "findings={:?} expectations={:?}",
                    r.finding_rule_ids, r.expectation_rule_ids
                )
            });
            let _ = writeln!(
                xml,
                "    <failure message=\"check failed\">{}</failure>",
                escape_xml(&detail)
            );
        }
        let _ = writeln!(xml, "  </testcase>");
    }
    let _ = writeln!(xml, "</testsuite>");
    std::fs::write(path, xml)
        .map_err(|err| EvalError::Io(format!("write junit {path:?}: {err}")))?;
    Ok(())
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
