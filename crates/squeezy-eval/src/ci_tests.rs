//! Tests for the input-token regression gate (cost idea B10b).

use std::path::PathBuf;

use super::*;

/// Build a minimal `ScenarioResult` that already passed the finding gates,
/// carrying the given observed input tokens. Used to drive the post-pass.
fn run_result(name: &str, observed: u64) -> ScenarioResult {
    ScenarioResult {
        name: name.to_string(),
        path: PathBuf::from(format!("{name}.toml")),
        passed: true,
        error: None,
        finding_rule_ids: vec![],
        expectation_rule_ids: vec![],
        elapsed_ms: 1,
        input_tokens: Some(observed),
        input_regression: None,
    }
}

#[test]
fn fail_on_parses_input_regression_keyword_variants() {
    for spec in ["input-regression", "input_regression", "input-tokens"] {
        let parsed = FailOn::parse(spec);
        assert!(parsed.input_regression, "spec {spec:?} should enable gate");
    }
    // Default policy never enables it.
    assert!(!FailOn::parse("expectations,errors").input_regression);
}

#[test]
fn evaluate_under_tolerance_passes_and_over_fails() {
    // Baseline 1000, 10% tolerance → limit 1100.
    // Exactly at the limit is OK.
    assert_eq!(
        evaluate_input_regression(1100, Some(1000), 0.10, true),
        InputRegressionVerdict::Ok {
            observed: 1100,
            baseline: 1000,
        }
    );
    // One token over the limit fails when the policy is on.
    assert_eq!(
        evaluate_input_regression(1101, Some(1000), 0.10, true),
        InputRegressionVerdict::Regressed {
            observed: 1101,
            baseline: 1000,
            limit: 1100,
            fail: true,
        }
    );
    // Same overshoot only warns when the policy is off.
    let warn = evaluate_input_regression(1101, Some(1000), 0.10, false);
    assert!(!warn.is_failure(), "{warn:?}");
    assert!(matches!(
        warn,
        InputRegressionVerdict::Regressed { fail: false, .. }
    ));
    // Big prefix blowups (2x) are unambiguously flagged.
    assert!(
        evaluate_input_regression(2000, Some(1000), 0.10, true).is_failure(),
        "2x regression must fail"
    );
}

#[test]
fn evaluate_without_baseline_records_not_fails() {
    let verdict = evaluate_input_regression(5_000, None, 0.10, true);
    assert_eq!(
        verdict,
        InputRegressionVerdict::Baselined { observed: 5_000 }
    );
    assert!(!verdict.is_failure());
}

#[test]
fn baseline_store_roundtrips_through_disk() {
    let dir = std::env::temp_dir().join(format!("sq-eval-baseline-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("nested").join("baseline.json");

    let mut baseline = InputBaseline::default();
    baseline.scenarios.insert("alpha".into(), 1234);
    baseline.scenarios.insert("beta".into(), 5678);
    baseline.save(&path).expect("save baseline");

    let loaded = InputBaseline::load(&path).expect("load baseline");
    assert_eq!(loaded.scenarios.get("alpha"), Some(&1234));
    assert_eq!(loaded.scenarios.get("beta"), Some(&5678));

    // Missing file loads as empty rather than erroring.
    let empty = InputBaseline::load(&dir.join("does-not-exist.json")).expect("load missing");
    assert!(empty.scenarios.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gate_inert_without_baseline_path() {
    let mut report = CheckReport {
        results: vec![run_result("scn", 9_999_999)],
    };
    // No baseline path configured → gate does nothing, run still passes.
    apply_input_regression_gate(&mut report, &InputRegression::default(), true).expect("gate runs");
    assert!(report.results[0].passed);
    assert!(report.results[0].input_regression.is_none());
}

#[test]
fn gate_fails_run_when_bytes_exceed_baseline_plus_tolerance() {
    let dir = std::env::temp_dir().join(format!("sq-eval-gate-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("baseline.json");
    let mut baseline = InputBaseline::default();
    baseline.scenarios.insert("scn".into(), 1_000);
    baseline.save(&path).expect("seed baseline");

    let cfg = InputRegression {
        baseline_path: Some(path.clone()),
        tolerance: 0.10,
        update_baseline: false,
    };

    // 1500 tokens vs baseline 1000 (+10% → limit 1100): regression.
    let mut report = CheckReport {
        results: vec![run_result("scn", 1_500)],
    };
    apply_input_regression_gate(&mut report, &cfg, true).expect("gate runs");
    assert!(!report.results[0].passed, "over-baseline run must fail");
    let verdict = report.results[0].input_regression.clone().expect("verdict");
    assert!(verdict.is_failure(), "{verdict:?}");
    assert!(verdict.message().contains("regression"), "{verdict:?}");

    // 1050 tokens is under the limit: same baseline, run passes.
    let mut under = CheckReport {
        results: vec![run_result("scn", 1_050)],
    };
    apply_input_regression_gate(&mut under, &cfg, true).expect("gate runs");
    assert!(under.results[0].passed, "under-baseline run must pass");
    assert!(matches!(
        under.results[0].input_regression,
        Some(InputRegressionVerdict::Ok { .. })
    ));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gate_warns_but_does_not_fail_when_policy_off() {
    let dir = std::env::temp_dir().join(format!("sq-eval-gate-warn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("baseline.json");
    let mut baseline = InputBaseline::default();
    baseline.scenarios.insert("scn".into(), 1_000);
    baseline.save(&path).expect("seed baseline");

    let cfg = InputRegression {
        baseline_path: Some(path),
        tolerance: 0.10,
        update_baseline: false,
    };

    let mut report = CheckReport {
        results: vec![run_result("scn", 5_000)],
    };
    // Policy off (false): a 5x blowup is reported but the run still passes,
    // so simply upgrading CI without opting in never breaks the build.
    apply_input_regression_gate(&mut report, &cfg, false).expect("gate runs");
    assert!(report.results[0].passed, "warn-only gate must not fail run");
    let verdict = report.results[0].input_regression.clone().expect("verdict");
    assert!(!verdict.is_failure(), "{verdict:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gate_records_new_baseline_when_update_enabled() {
    let dir = std::env::temp_dir().join(format!("sq-eval-gate-record-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("baseline.json");

    let cfg = InputRegression {
        baseline_path: Some(path.clone()),
        tolerance: 0.10,
        update_baseline: true,
    };

    let mut report = CheckReport {
        results: vec![run_result("fresh", 4_242)],
    };
    apply_input_regression_gate(&mut report, &cfg, true).expect("gate runs");
    // First sighting: recorded, never a failure.
    assert!(report.results[0].passed);
    assert!(matches!(
        report.results[0].input_regression,
        Some(InputRegressionVerdict::Baselined { observed: 4_242 })
    ));

    // The value was persisted for the next run.
    let stored = InputBaseline::load(&path).expect("load");
    assert_eq!(stored.scenarios.get("fresh"), Some(&4_242));

    let _ = std::fs::remove_dir_all(&dir);
}
