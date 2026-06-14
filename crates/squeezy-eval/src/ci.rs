//! `squeezy-eval check` — CI entry point.
//!
//! Iterates every `*.toml` scenario in a directory, runs each with
//! `--no-triage`, and exits non-zero when any scenario violates the
//! requested `fail_on` policy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
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
    /// Optional input-token regression gate. Inert when `baseline_path` is
    /// `None` (the default), so existing `check` invocations are unaffected.
    pub input_regression: InputRegression,
    /// When true, run only scenarios that need no provider key or external
    /// setup: `provider = "mock"` and `hermetic = true`. Everything else is
    /// skipped (and logged). Used by the offline CI job.
    pub hermetic_only: bool,
    /// Emit one line as each scenario completes. This keeps long CI runs from
    /// looking wedged while preserving the final sorted report.
    pub emit_progress: bool,
}

#[derive(Debug, Clone, Default)]
pub struct FailOn {
    pub findings: bool,
    pub expectations: bool,
    pub errors: bool,
    /// Opt-in: when a run's provider-reported input tokens exceed the stored
    /// per-scenario baseline by more than the configured tolerance, treat it
    /// as a failure. Off by default so existing CI is never broken by simply
    /// upgrading; enable with `--fail-on input-regression`. When this keyword
    /// is absent the gate still *reports* (warns) but never fails the run.
    pub input_regression: bool,
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
            } else if part.eq_ignore_ascii_case("input-regression")
                || part.eq_ignore_ascii_case("input_regression")
                || part.eq_ignore_ascii_case("input-tokens")
            {
                fail_on.input_regression = true;
            }
        }
        fail_on
    }
}

/// Default fraction a run may exceed its baseline input-token count before
/// the regression gate fires. 0.10 == 10%. Chosen to absorb provider-side
/// tokenizer jitter and minor prompt churn while still catching the 2×–200×
/// prefix blowups that prompt/retrieval drift causes.
pub const DEFAULT_INPUT_TOLERANCE: f64 = 0.10;

/// Configuration for the input-byte/token regression gate. Fully optional:
/// when `baseline_path` is `None` the gate is inert and `run_check` behaves
/// exactly as before.
#[derive(Debug, Clone, Default)]
pub struct InputRegression {
    /// JSON file mapping scenario id → baseline input-token count. Missing
    /// file or missing entry means "no baseline yet": the run is recorded as
    /// the new baseline and never fails.
    pub baseline_path: Option<PathBuf>,
    /// Fraction over baseline that is tolerated before flagging (e.g. 0.10).
    pub tolerance: f64,
    /// When true, a freshly observed (previously-unbaselined) scenario is
    /// written back into the baseline file so the next run has something to
    /// compare against. When false the file is treated as read-only.
    pub update_baseline: bool,
}

/// On-disk baseline store: scenario id → recorded input-token count. Stored
/// as a stable, sorted JSON object so diffs in version control are minimal.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InputBaseline {
    #[serde(default)]
    pub scenarios: BTreeMap<String, u64>,
}

impl InputBaseline {
    pub fn load(path: &Path) -> Result<Self, EvalError> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|err| EvalError::Io(format!("parse baseline {path:?}: {err}"))),
            // A missing baseline file is the expected first-run state, not an
            // error: start empty and let the run populate it.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(EvalError::Io(format!("read baseline {path:?}: {err}"))),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), EvalError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|err| EvalError::Io(format!("create baseline dir {parent:?}: {err}")))?;
        }
        let json = serde_json::to_vec_pretty(self)
            .map_err(|err| EvalError::Internal(format!("serialize baseline: {err}")))?;
        std::fs::write(path, json)
            .map_err(|err| EvalError::Io(format!("write baseline {path:?}: {err}")))
    }
}

/// Outcome of comparing one run's input tokens against its baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputRegressionVerdict {
    /// No baseline recorded yet for this scenario; the observed value becomes
    /// the baseline. Never a failure.
    Baselined { observed: u64 },
    /// Within tolerance of the baseline.
    Ok { observed: u64, baseline: u64 },
    /// Exceeded baseline + tolerance. `fail` reflects the active policy (true
    /// only when `--fail-on input-regression` was set); otherwise it is a warn.
    Regressed {
        observed: u64,
        baseline: u64,
        limit: u64,
        fail: bool,
    },
}

impl InputRegressionVerdict {
    /// True only for a real, policy-enabled failure.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Regressed { fail: true, .. })
    }

    /// Human-readable one-liner for the CLI / JUnit detail.
    pub fn message(&self) -> String {
        match self {
            Self::Baselined { observed } => {
                format!("input-tokens baseline recorded: {observed}")
            }
            Self::Ok { observed, baseline } => {
                format!("input-tokens {observed} within baseline {baseline}")
            }
            Self::Regressed {
                observed,
                baseline,
                limit,
                fail,
            } => {
                let kind = if *fail { "FAIL" } else { "warn" };
                format!(
                    "input-tokens regression ({kind}): {observed} exceeds baseline {baseline} + tolerance (limit {limit})"
                )
            }
        }
    }
}

/// Pure comparison: given the observed input tokens, an optional baseline,
/// a tolerance fraction, and whether the policy is set to fail, decide the
/// verdict. No I/O — the caller owns baseline loading/persisting so this is
/// trivially unit-testable.
pub fn evaluate_input_regression(
    observed: u64,
    baseline: Option<u64>,
    tolerance: f64,
    fail_policy: bool,
) -> InputRegressionVerdict {
    let Some(baseline) = baseline else {
        return InputRegressionVerdict::Baselined { observed };
    };
    // Clamp negative tolerances to 0 so a misconfigured value can only make
    // the gate stricter, never silently disable it.
    let tolerance = tolerance.max(0.0);
    // Round the limit up so an exactly-on-tolerance run is treated as OK
    // (the boundary is inclusive of the tolerated headroom).
    let limit = ((baseline as f64) * (1.0 + tolerance)).floor() as u64;
    if observed > limit {
        InputRegressionVerdict::Regressed {
            observed,
            baseline,
            limit,
            fail: fail_policy,
        }
    } else {
        InputRegressionVerdict::Ok { observed, baseline }
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
                input_regression: false,
            },
            junit_path: None,
            parallelism: None,
            input_regression: InputRegression::default(),
            hermetic_only: false,
            emit_progress: false,
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
    /// Provider-reported input tokens for this run, when the scenario ran.
    /// `None` for load/parse/panic failures that never reached a run.
    pub input_tokens: Option<u64>,
    /// Verdict from the input-token regression gate, when it was active.
    pub input_regression: Option<InputRegressionVerdict>,
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

    // Hermetic mode (CI): keep only scenarios that run with no provider key
    // or external setup — `provider = "mock"` and `hermetic = true`. Skips are
    // logged, never silent, so a shrinking suite can't masquerade as green.
    if opts.hermetic_only {
        let total = entries.len();
        let mut kept = Vec::with_capacity(entries.len());
        let mut skipped: Vec<String> = Vec::new();
        for path in entries {
            match scenario::load(&path) {
                Ok(s) => {
                    let is_mock = s.squeezy.provider.as_deref() == Some("mock");
                    if is_mock && s.hermetic {
                        kept.push(path);
                    } else {
                        let reason = if is_mock {
                            "hermetic = false"
                        } else {
                            "non-mock provider"
                        };
                        skipped.push(format!("{} ({reason})", path.display()));
                    }
                }
                // Let the normal run path surface a parse error rather than
                // hiding it behind a skip.
                Err(_) => kept.push(path),
            }
        }
        if !skipped.is_empty() {
            eprintln!(
                "check --hermetic: running {} of {total} scenarios; skipped {}:",
                kept.len(),
                skipped.len()
            );
            for s in &skipped {
                eprintln!("  - skip {s}");
            }
        }
        entries = kept;
    }

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
                        input_tokens: None,
                        input_regression: None,
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
                        // Regression verdict is applied in a post-pass that
                        // owns the loaded baseline; here we only carry the raw
                        // observation so that pass stays deterministic.
                        input_tokens: Some(outcome.input_tokens),
                        input_regression: None,
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
                    input_tokens: None,
                    input_regression: None,
                },
            }
        });
        handles.push(handle);
    }

    let mut report = CheckReport::default();
    for handle in handles {
        match handle.await {
            Ok(result) => {
                if opts.emit_progress {
                    emit_progress(&result);
                }
                report.results.push(result);
            }
            Err(err) => {
                let result = ScenarioResult {
                    name: "<panicked>".into(),
                    path: PathBuf::new(),
                    passed: !opts.fail_on.errors,
                    error: Some(format!("scenario task panicked: {err}")),
                    finding_rule_ids: vec![],
                    expectation_rule_ids: vec![],
                    elapsed_ms: 0,
                    input_tokens: None,
                    input_regression: None,
                };
                if opts.emit_progress {
                    emit_progress(&result);
                }
                report.results.push(result);
            }
        }
    }
    // Restore deterministic ordering (parallel tasks can finish out of order).
    report.results.sort_by(|a, b| a.path.cmp(&b.path));

    // Input-token regression gate (opt-in via a configured baseline path).
    // Runs as a post-pass so it owns the single loaded baseline and the
    // writeback, instead of cloning state into every scenario task.
    apply_input_regression_gate(
        &mut report,
        &opts.input_regression,
        opts.fail_on.input_regression,
    )?;

    if let Some(junit_path) = &opts.junit_path {
        write_junit(junit_path, &report)?;
    }
    Ok(report)
}

fn emit_progress(result: &ScenarioResult) {
    let status = if result.passed { "PASS" } else { "FAIL" };
    eprintln!(
        "check progress: {status} {:<40} {}ms",
        result.name, result.elapsed_ms
    );
}

/// Compare each run's input tokens against the stored baseline, annotate the
/// results, flip `passed` to false on a policy-enabled regression, and (when
/// `update_baseline`) persist any newly-observed scenarios. Inert when no
/// `baseline_path` is configured.
fn apply_input_regression_gate(
    report: &mut CheckReport,
    cfg: &InputRegression,
    fail_policy: bool,
) -> Result<(), EvalError> {
    let Some(baseline_path) = cfg.baseline_path.as_ref() else {
        return Ok(());
    };
    let tolerance = if cfg.tolerance > 0.0 {
        cfg.tolerance
    } else {
        DEFAULT_INPUT_TOLERANCE
    };
    let mut baseline = InputBaseline::load(baseline_path)?;
    let mut dirty = false;

    for result in &mut report.results {
        let Some(observed) = result.input_tokens else {
            continue;
        };
        let prior = baseline.scenarios.get(&result.name).copied();
        let verdict = evaluate_input_regression(observed, prior, tolerance, fail_policy);
        match &verdict {
            InputRegressionVerdict::Baselined { observed } => {
                // First sighting: record it so the next run has a reference.
                if cfg.update_baseline {
                    baseline.scenarios.insert(result.name.clone(), *observed);
                    dirty = true;
                }
            }
            InputRegressionVerdict::Regressed { fail: true, .. } => {
                result.passed = false;
            }
            // Within tolerance or warn-only regression: report, don't fail.
            InputRegressionVerdict::Ok { .. } | InputRegressionVerdict::Regressed { .. } => {}
        }
        result.input_regression = Some(verdict);
    }

    if dirty {
        baseline.save(baseline_path)?;
    }
    Ok(())
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
                if let Some(verdict) = r.input_regression.as_ref().filter(|v| v.is_failure()) {
                    verdict.message()
                } else {
                    format!(
                        "findings={:?} expectations={:?}",
                        r.finding_rule_ids, r.expectation_rule_ids
                    )
                }
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

#[cfg(test)]
#[path = "ci_tests.rs"]
mod tests;
