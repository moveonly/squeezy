//! `squeezy-eval check` — CI entry point.
//!
//! Iterates every `*.toml` scenario in a directory, runs each with
//! `--no-triage`, and exits non-zero when any scenario violates the
//! requested `fail_on` policy.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::driver::{EvalError, RunOptions, run_scenario};
use crate::scenario;

#[derive(Debug, Clone)]
pub struct CheckOptions {
    pub dir: PathBuf,
    pub out_root: PathBuf,
    pub fail_on: FailOn,
    pub junit_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct FailOn {
    pub findings: bool,
    pub expectations: bool,
    pub errors: bool,
}

impl FailOn {
    pub fn parse(spec: &str) -> Self {
        let parts: BTreeSet<String> = spec
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        Self {
            findings: parts.contains("findings"),
            // Expectations are a subset of auto-findings (rules whose
            // ids start with `expect_`); treat the keyword as opt-in
            // separately from the broader `findings` bucket.
            expectations: parts.contains("expectations"),
            errors: parts.contains("errors"),
        }
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
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&opts.dir)
        .map_err(|err| EvalError::Io(format!("read_dir {:?}: {err}", opts.dir)))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("toml"))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();

    let mut report = CheckReport::default();
    for path in entries {
        let started = std::time::Instant::now();
        let scenario = match scenario::load(&path) {
            Ok(s) => s,
            Err(err) => {
                report.results.push(ScenarioResult {
                    name: path.display().to_string(),
                    path,
                    passed: !opts.fail_on.errors,
                    error: Some(format!("{err}")),
                    finding_rule_ids: vec![],
                    expectation_rule_ids: vec![],
                    elapsed_ms: started.elapsed().as_millis(),
                });
                continue;
            }
        };
        let name = scenario.id.clone();
        let run_options = RunOptions {
            scenario_path: path.clone(),
            out_root: opts.out_root.clone(),
            run_triage: false,
            emit_github: false,
            gh_repo: None,
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
                if opts.fail_on.expectations && !expectation_rule_ids.is_empty() {
                    passed = false;
                }
                if opts.fail_on.findings && !finding_rule_ids.is_empty() {
                    passed = false;
                }
                report.results.push(ScenarioResult {
                    name,
                    path,
                    passed,
                    error: None,
                    finding_rule_ids,
                    expectation_rule_ids,
                    elapsed_ms: started.elapsed().as_millis(),
                });
            }
            Err(err) => {
                report.results.push(ScenarioResult {
                    name,
                    path,
                    passed: !opts.fail_on.errors,
                    error: Some(format!("{err}")),
                    finding_rule_ids: vec![],
                    expectation_rule_ids: vec![],
                    elapsed_ms: started.elapsed().as_millis(),
                });
            }
        }
    }

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
