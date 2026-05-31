use squeezy_core::{Result, SqueezyError};

use crate::oracles::scala_semanticdb::{SCALA_SCAN_ONLY_PREFIX, SCALA_SEMANTICDB_STATUS_PREFIX};
use crate::report::BenchmarkReport;

/// Returns true when the Scala oracle ran end-to-end against SemanticDB
/// protobufs. The gate is suppressed when scalac was missing or its
/// invocation failed (scan-only fallback) so CI runners without a Scala
/// toolchain still pass; once the toolchain is in place the precision /
/// recall thresholds become load-bearing.
fn scala_gate_active(status: &str) -> bool {
    if status.starts_with(SCALA_SCAN_ONLY_PREFIX) {
        return false;
    }
    if status.starts_with("skipped") {
        return false;
    }
    status.starts_with(SCALA_SEMANTICDB_STATUS_PREFIX)
}

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

    // Kotlin smoke-tier symbol parity gate. Thresholds from
    // `target/lang-specs/kotlin.md` §10: precision >= 0.94, recall >= 0.85.
    // Skipped when the oracle was unavailable so missing kotlinc/JDK doesn't
    // mask the fixture-query gates above.
    if !no_speed_gate
        && let Some(kotlin) = &report.kotlin_oracle
        && kotlin.oracle_ms.is_some()
    {
        if kotlin.symbols.precision < 0.94 {
            return Err(SqueezyError::Graph(format!(
                "Kotlin oracle precision below 0.94 floor: {:.3} (fp={})",
                kotlin.symbols.precision, kotlin.symbols.false_positive,
            )));
        }
        if kotlin.symbols.recall < 0.85 {
            return Err(SqueezyError::Graph(format!(
                "Kotlin oracle recall below 0.85 floor: {:.3} (fn={})",
                kotlin.symbols.recall, kotlin.symbols.false_negative,
            )));
        }
    }

    if !no_speed_gate
        && let Some(php) = &report.php_oracle
        && php.oracle_ms.is_some()
    {
        const PHP_PRECISION_FLOOR: f64 = 0.92;
        const PHP_RECALL_FLOOR: f64 = 0.80;
        if php.symbols.precision < PHP_PRECISION_FLOOR {
            return Err(SqueezyError::Graph(format!(
                "PHP oracle precision {:.3} below floor {:.2}",
                php.symbols.precision, PHP_PRECISION_FLOOR
            )));
        }
        if php.symbols.recall < PHP_RECALL_FLOOR {
            return Err(SqueezyError::Graph(format!(
                "PHP oracle recall {:.3} below floor {:.2}",
                php.symbols.recall, PHP_RECALL_FLOOR
            )));
        }
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

    if !no_speed_gate
        && let Some(scala) = &report.scala_oracle
        && scala_gate_active(&scala.status)
        && (scala.symbols.precision < 0.90 || scala.symbols.recall < 0.75)
    {
        return Err(SqueezyError::Graph(format!(
            "Scala oracle accuracy below gate: precision={:.3} recall={:.3}",
            scala.symbols.precision, scala.symbols.recall
        )));
    }

    // Spec §10: Swift first-PR thresholds. precision >= 0.92, recall >=
    // 0.80. The speed gate stays disabled per the corpus entry
    // (`no_speed_gate: true`). We enforce thresholds only when the
    // oracle was actually invoked.
    if let Some(swift) = &report.swift_oracle {
        let precision = swift.symbols.precision;
        let recall = swift.symbols.recall;
        let denom = swift.symbols.true_positive
            + swift.symbols.false_positive
            + swift.symbols.false_negative;
        if denom > 0 {
            if precision < 0.92 {
                return Err(SqueezyError::Graph(format!(
                    "Swift symbol precision {precision:.3} below 0.92 gate (tp={} fp={} fn={})",
                    swift.symbols.true_positive,
                    swift.symbols.false_positive,
                    swift.symbols.false_negative,
                )));
            }
            if recall < 0.80 {
                return Err(SqueezyError::Graph(format!(
                    "Swift symbol recall {recall:.3} below 0.80 gate (tp={} fp={} fn={})",
                    swift.symbols.true_positive,
                    swift.symbols.false_positive,
                    swift.symbols.false_negative,
                )));
            }
        }
        // Spec §10: navigation probe gates. Only enforce when probes
        // actually ran (sourcekit-lsp present + probe_limit > 0). When
        // the LSP is unavailable the probe report is empty and the
        // accuracy stays at the f64 default of 1.0; gate that off the
        // `probes` count so a missing toolchain does not trip the
        // gate. Mirrors the symbol gate pattern above and the existing
        // Rust-side nav-accuracy treatment (no hard gate — observed by
        // hand on rust corpus today).
        let nav = &swift.navigation_accuracy;
        if nav.definitions.probes > 0 && nav.definitions.precision < 0.85 {
            return Err(SqueezyError::Graph(format!(
                "Swift definition precision {:.3} below 0.85 gate (probes={} tp={} fp={} fn={})",
                nav.definitions.precision,
                nav.definitions.probes,
                nav.definitions.true_positive,
                nav.definitions.false_positive,
                nav.definitions.false_negative,
            )));
        }
        // Precision = tp / (tp + fp); when (tp + fp) < 5 each emission
        // shifts the ratio by ≥20% and the gate is statistical noise. This
        // matters in practice because SourceKit-LSP without a SwiftPM build
        // index frequently returns zero references for sampled
        // declarations, so legitimate type-position emissions (return
        // types, protocol-conformance clauses) end up the only signal — a
        // denominator too thin to gate on.
        const SWIFT_REF_GATE_MIN_EMISSIONS: usize = 5;
        let ref_emissions = nav.references.true_positive + nav.references.false_positive;
        if ref_emissions >= SWIFT_REF_GATE_MIN_EMISSIONS && nav.references.precision < 0.80 {
            return Err(SqueezyError::Graph(format!(
                "Swift reference precision {:.3} below 0.80 gate (symbols={} tp={} fp={} fn={})",
                nav.references.precision,
                nav.references.symbols_sampled,
                nav.references.true_positive,
                nav.references.false_positive,
                nav.references.false_negative,
            )));
        }
    }

    // Dart oracle gate (spec dart.md §10). Threshold is `precision >= 0.93`
    // and `recall >= 0.85`; the scan-only fallback (dart toolchain missing)
    // is suppressed because there is no oracle signal to compare against.
    if !no_speed_gate
        && let Some(dart) = &report.dart_oracle
        && dart.mode == "analyzer"
        && (dart.symbols.precision < 0.93 || dart.symbols.recall < 0.85)
    {
        return Err(SqueezyError::Graph(format!(
            "Dart oracle accuracy regressed: precision={:.3} recall={:.3} (mode={})",
            dart.symbols.precision, dart.symbols.recall, dart.mode
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
