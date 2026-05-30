use squeezy_core::{Result, SqueezyError};

use crate::report::BenchmarkReport;

pub(crate) fn enforce_gates(report: &BenchmarkReport, no_speed_gate: bool) -> Result<()> {
    let missing = report
        .queries
        .iter()
        .flat_map(|query| {
            query
                .missing
                .iter()
                .map(|missing| format!("{} missing {missing}", query.id))
        })
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(SqueezyError::Graph(format!(
            "benchmark expected results missing: {}",
            missing.join(", ")
        )));
    }

    if !no_speed_gate && !report.faster_than_validation {
        return Err(SqueezyError::Graph(format!(
            "Squeezy graph was not faster than {} validation: {}ms >= {}ms",
            report.validation_status, report.squeezy_total_ms, report.validation_ms
        )));
    }

    if let Some(refresh) = &report.refresh_probe
        && refresh.reparsed_files != refresh.edited_files
    {
        return Err(SqueezyError::Graph(format!(
            "refresh probe reparsed {} files after {} edits",
            refresh.reparsed_files, refresh.edited_files
        )));
    }

    if !no_speed_gate
        && let Some(go) = &report.go_oracle
        && (go.symbols.false_positive != 0 || go.symbols.false_negative != 0)
    {
        return Err(SqueezyError::Graph(format!(
            "Go oracle accuracy regressed: fp={} fn={}",
            go.symbols.false_positive, go.symbols.false_negative
        )));
    }

    // Ruby uses precision/recall thresholds rather than fp/fn counts because
    // the dynamic-dispatch recall gap (`method_missing`, `define_method`)
    // produces a steady stream of FNs we accept (spec §10). When the oracle
    // is in `scan-only` fallback the precision/recall are 1.0 by
    // construction; the gate still passes but the report's `mode` field
    // tells consumers the oracle didn't run.
    if !no_speed_gate
        && let Some(ruby) = &report.ruby_oracle
        && (ruby.symbols.precision < 0.90 || ruby.symbols.recall < 0.75)
    {
        return Err(SqueezyError::Graph(format!(
            "Ruby oracle accuracy regressed: precision={:.3} recall={:.3}",
            ruby.symbols.precision, ruby.symbols.recall
        )));
    }

    if let Some(mixed) = &report.mixed_workload
        && mixed.refresh_probe.reparsed_files != mixed.refresh_probe.edited_files
    {
        return Err(SqueezyError::Graph(format!(
            "refresh probe reparsed {} files after {} edits",
            mixed.refresh_probe.reparsed_files, mixed.refresh_probe.edited_files
        )));
    }

    Ok(())
}
