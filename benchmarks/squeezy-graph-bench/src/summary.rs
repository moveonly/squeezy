use std::{fs, path::Path};

use squeezy_core::{Result, SqueezyError};

use crate::report::*;

pub(crate) fn write_report(path: &Path, report: &BenchmarkReport) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(report)
        .map_err(|err| SqueezyError::Graph(format!("failed to serialize report: {err}")))?;
    fs::write(path, format!("{text}\n"))?;
    Ok(())
}

pub(crate) fn print_summary(report: &BenchmarkReport) {
    println!("semantic graph benchmark");
    println!("language: {}", report.language);
    println!("fixture: {}", report.fixture);
    println!(
        "validation: {} ({}ms)",
        report.validation_status, report.validation_ms
    );
    println!("squeezy_total_ms: {}", report.squeezy_total_ms);
    println!(
        "build_phases: crawl={}ms parse={}ms declaration_graph={}ms full_graph={}ms total={}ms",
        report.build_phases.crawl_ms,
        report.build_phases.parse_ms,
        report.build_phases.declaration_graph_ms,
        report.build_phases.full_graph_ms,
        report.build_phases.total_ms
    );
    println!(
        "graph_indexes: body_hit_trigram_indexed={} body_hit_trigram_terms={} reference_index_terms={}",
        report.graph.body_hit_trigram_indexed,
        report.graph.body_hit_trigram_terms,
        report.graph.reference_index_terms
    );
    println!("faster_than_validation: {}", report.faster_than_validation);
    print_accuracy_summary("fixture", &report.accuracy);
    print_navigation_summary("fixture", &report.accuracy.navigation);
    if let Some(python) = &report.python_oracle {
        println!(
            "python_oracle_symbol_accuracy: tp={} fp={} fn={} precision={} recall={} oracle_symbols={} squeezy_symbols={} oracle={}ms oracle_unparseable={}",
            python.symbols.true_positive,
            python.symbols.false_positive,
            python.symbols.false_negative,
            python.symbols.precision,
            python.symbols.recall,
            python.symbols.rust_analyzer_total,
            python.symbols.squeezy_total,
            python.oracle_ms,
            python.oracle_unparseable_files
        );
    }
    if let Some(js_ts) = &report.js_ts_oracle {
        println!(
            "js_ts_oracle_symbol_accuracy: tp={} fp={} fn={} precision={} recall={} oracle_symbols={} squeezy_symbols={} oracle={}ms status={}",
            js_ts.symbols.true_positive,
            js_ts.symbols.false_positive,
            js_ts.symbols.false_negative,
            js_ts.symbols.precision,
            js_ts.symbols.recall,
            js_ts.symbols.rust_analyzer_total,
            js_ts.symbols.squeezy_total,
            js_ts.oracle_ms,
            js_ts.status
        );
    }
    if let Some(java) = &report.java_oracle {
        println!(
            "java_oracle_symbol_accuracy: tp={} fp={} fn={} precision={} recall={} oracle_symbols={} squeezy_symbols={} oracle={}",
            java.symbols.true_positive,
            java.symbols.false_positive,
            java.symbols.false_negative,
            java.symbols.precision,
            java.symbols.recall,
            java.symbols.rust_analyzer_total,
            java.symbols.squeezy_total,
            java.oracle_ms
                .map(|ms| format!("{ms}ms"))
                .unwrap_or_else(|| java.status.clone())
        );
        println!(
            "java_oracle_navigation_accuracy: queries={} tp={} fp={} fn={} precision={} recall={} oracle={}",
            java.navigation.query_count,
            java.navigation.true_positive,
            java.navigation.false_positive,
            java.navigation.false_negative,
            java.navigation.precision,
            java.navigation.recall,
            java.navigation.status
        );
    }
    if let Some(csharp) = &report.csharp_oracle {
        println!(
            "csharp_oracle_symbol_accuracy: tp={} fp={} fn={} precision={} recall={} oracle_symbols={} squeezy_symbols={} oracle={}ms build={} oracle_unparseable={}",
            csharp.symbols.true_positive,
            csharp.symbols.false_positive,
            csharp.symbols.false_negative,
            csharp.symbols.precision,
            csharp.symbols.recall,
            csharp.symbols.rust_analyzer_total,
            csharp.symbols.squeezy_total,
            csharp.oracle_ms,
            csharp
                .oracle_build_ms
                .map(|ms| format!("{ms}ms"))
                .unwrap_or_else(|| "cached".to_string()),
            csharp.oracle_unparseable_files,
        );
    }
    if let Some(go) = &report.go_oracle {
        println!(
            "go_oracle_symbol_accuracy: tp={} fp={} fn={} precision={} recall={} oracle_symbols={} squeezy_symbols={} oracle={}ms oracle_unparseable={}",
            go.symbols.true_positive,
            go.symbols.false_positive,
            go.symbols.false_negative,
            go.symbols.precision,
            go.symbols.recall,
            go.symbols.rust_analyzer_total,
            go.symbols.squeezy_total,
            go.oracle_ms,
            go.oracle_unparseable_files
        );
    }
    if let Some(refresh) = &report.refresh_probe {
        println!(
            "refresh_probe: language={} copied={} edited={} reparsed={} refresh_ms={} budget_exhausted={}",
            refresh.language,
            refresh.copied_source_files,
            refresh.edited_files,
            refresh.reparsed_files,
            refresh.refresh_ms,
            refresh.budget_exhausted
        );
    }
    for iteration in &report.heuristic_iterations {
        println!(
            "heuristic_iteration: {} status={}",
            iteration.name, iteration.status
        );
    }
    for query in &report.queries {
        println!(
            "{}: actual={} missing={} extras={}",
            query.id,
            query.actual.len(),
            query.missing.len(),
            query.extras.len()
        );
    }
    if let Some(mixed) = &report.mixed_workload {
        println!("mixed_repo: {}", mixed.repo);
        println!("mixed_requested_scenarios: {}", mixed.requested_scenarios);
        println!("mixed_available_scenarios: {}", mixed.available_scenarios);
        println!("mixed_executed_scenarios: {}", mixed.executed_scenarios);
        println!("mixed_tools: {}", mixed.tools.join(", "));
        println!(
            "mixed_compiler_check: {}",
            mixed
                .compiler_check_ms
                .map(|ms| format!("{ms}ms"))
                .unwrap_or_else(|| mixed.compiler_check_status.clone())
        );
        println!("mixed_squeezy_total_ms: {}", mixed.squeezy_total_ms);
        println!("mixed_query_time_ms: {:?}", mixed.query_time_ms);
        println!("mixed_refresh_ms: {}", mixed.refresh_probe.refresh_ms);
        println!(
            "mixed_rust_analyzer: {}",
            mixed
                .rust_analyzer_ms
                .map(|ms| format!("{ms}ms"))
                .unwrap_or_else(|| mixed.rust_analyzer_status.clone())
        );
        print_accuracy_summary("mixed", &mixed.accuracy);
        print_navigation_summary("mixed", &mixed.accuracy.navigation);
    }
}

pub(crate) fn print_accuracy_summary(label: &str, accuracy: &AccuracyReport) {
    println!(
        "{label}_symbol_accuracy: tp={} fp={} fn={} precision={} recall={} comparable_ra={} raw_ra={} excluded_ra={:?} comparable_squeezy={} raw_squeezy={} excluded_squeezy={:?} ra_symbols={}",
        accuracy.symbols.true_positive,
        accuracy.symbols.false_positive,
        accuracy.symbols.false_negative,
        accuracy.symbols.precision,
        accuracy.symbols.recall,
        accuracy.symbols.rust_analyzer_total,
        accuracy.symbols.rust_analyzer_raw_total,
        accuracy.symbols.rust_analyzer_excluded_by_kind,
        accuracy.symbols.squeezy_total,
        accuracy.symbols.squeezy_raw_total,
        accuracy.symbols.squeezy_excluded_by_kind,
        accuracy
            .rust_analyzer_symbols_ms
            .map(|ms| format!("{ms}ms"))
            .unwrap_or_else(|| accuracy.rust_analyzer_symbol_status.clone())
    );
}

pub(crate) fn print_navigation_summary(label: &str, navigation: &NavigationAccuracyReport) {
    println!(
        "{label}_navigation_accuracy: def_probes={}/{} def_tp={} def_fp={} def_fn={} def_squeezy_only={} def_wrong_target={} ref_symbols={}/{} ref_tp={} ref_fp={} ref_fn={} lsp={}",
        navigation.definitions.probes,
        navigation.definitions.available_probes,
        navigation.definitions.true_positive,
        navigation.definitions.false_positive,
        navigation.definitions.false_negative,
        navigation.definitions.squeezy_only,
        navigation.definitions.wrong_target,
        navigation.references.symbols_sampled,
        navigation.references.available_symbols,
        navigation.references.true_positive,
        navigation.references.false_positive,
        navigation.references.false_negative,
        navigation
            .rust_analyzer_lsp_ms
            .map(|ms| format!("{ms}ms"))
            .unwrap_or_else(|| navigation.rust_analyzer_lsp_status.clone())
    );
}

