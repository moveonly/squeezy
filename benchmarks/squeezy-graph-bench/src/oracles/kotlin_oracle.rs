use std::{path::Path, process::Command, time::Instant};

use serde::Deserialize;
use squeezy_core::{Result, SqueezyError};
use squeezy_graph::SemanticGraph;

use crate::{
    accuracy::{compare_symbol_sets, increment_symbol, ratio, symbol_count},
    oracles::common_scan::{collect_kotlin_squeezy_symbol_scan, default_oracle_exclusions},
    oracles::rust_analyzer::normalize_symbol_name,
    report::{KotlinOracleReport, QueryOracleReport, QueryReport, SymbolKey, SymbolScan},
    util::{command_exists, increment},
};

/// Path of the Kotlin oracle jar relative to the repo root. Built by
/// `benchmarks/oracle/kotlin/build.sh` ahead of the benchmark run; not
/// checked into the repo because the resulting fat jar (kotlinc
/// `-include-runtime`) is ~20MB.
const KOTLIN_ORACLE_JAR: &str = "benchmarks/oracle/kotlin/kotlin-oracle.jar";

pub(crate) fn time_kotlin_oracle_optional(root: &Path) -> (u128, String) {
    if !command_exists("java") {
        return (0, "skipped: java not found".to_string());
    }
    if !Path::new(KOTLIN_ORACLE_JAR).exists() {
        return (
            0,
            "skipped: Kotlin oracle jar not built (run benchmarks/oracle/kotlin/build.sh)"
                .to_string(),
        );
    }
    let started = Instant::now();
    match collect_kotlin_compiler_tree_symbol_scan(root) {
        Ok((_, status)) if status.starts_with("Kotlin oracle succeeded") => {
            (started.elapsed().as_millis(), status)
        }
        Ok((_, status)) => (0, format!("skipped: {status}")),
        Err(err) => (0, format!("skipped: Kotlin oracle failed: {err}")),
    }
}

pub(crate) fn collect_kotlin_oracle_accuracy(
    root: &Path,
    graph: &SemanticGraph,
    queries: &[QueryReport],
) -> Result<KotlinOracleReport> {
    if !command_exists("java") {
        return Ok(KotlinOracleReport {
            oracle_ms: None,
            status: "skipped: java not found".to_string(),
            symbols: compare_symbol_sets(
                &collect_kotlin_squeezy_symbol_scan(graph),
                &SymbolScan::default(),
            ),
            navigation: collect_kotlin_query_oracle_accuracy(queries),
            limitations: kotlin_oracle_limitations(),
        });
    }
    if !Path::new(KOTLIN_ORACLE_JAR).exists() {
        return Ok(KotlinOracleReport {
            oracle_ms: None,
            status:
                "skipped: Kotlin oracle jar not built (run benchmarks/oracle/kotlin/build.sh)"
                    .to_string(),
            symbols: compare_symbol_sets(
                &collect_kotlin_squeezy_symbol_scan(graph),
                &SymbolScan::default(),
            ),
            navigation: collect_kotlin_query_oracle_accuracy(queries),
            limitations: kotlin_oracle_limitations(),
        });
    }
    let started = Instant::now();
    match collect_kotlin_compiler_tree_symbol_scan(root) {
        Ok((oracle, status)) if status.starts_with("Kotlin oracle succeeded") => {
            let oracle_ms = started.elapsed().as_millis();
            let squeezy_symbols = collect_kotlin_squeezy_symbol_scan(graph);
            Ok(KotlinOracleReport {
                oracle_ms: Some(oracle_ms),
                status,
                symbols: compare_symbol_sets(&squeezy_symbols, &oracle),
                navigation: collect_kotlin_query_oracle_accuracy(queries),
                limitations: kotlin_oracle_limitations(),
            })
        }
        Ok((_, status)) => Ok(KotlinOracleReport {
            oracle_ms: None,
            status: format!("skipped: {status}"),
            symbols: compare_symbol_sets(
                &collect_kotlin_squeezy_symbol_scan(graph),
                &SymbolScan::default(),
            ),
            navigation: collect_kotlin_query_oracle_accuracy(queries),
            limitations: kotlin_oracle_limitations(),
        }),
        Err(err) => Ok(KotlinOracleReport {
            oracle_ms: None,
            status: format!("skipped: Kotlin oracle failed: {err}"),
            symbols: compare_symbol_sets(
                &collect_kotlin_squeezy_symbol_scan(graph),
                &SymbolScan::default(),
            ),
            navigation: collect_kotlin_query_oracle_accuracy(queries),
            limitations: kotlin_oracle_limitations(),
        }),
    }
}

pub(crate) fn collect_kotlin_query_oracle_accuracy(queries: &[QueryReport]) -> QueryOracleReport {
    // Identical scoring logic to javac.rs collect_query_oracle_accuracy: per-
    // query expected_contains is the truth set, missing entries are FN,
    // extras are tolerated (not counted as FP).
    let true_positive = queries
        .iter()
        .map(|query| {
            query
                .expected_contains
                .iter()
                .filter(|expected| query.actual.contains(expected))
                .count()
        })
        .sum::<usize>();
    let false_negative = queries
        .iter()
        .map(|query| query.missing.len())
        .sum::<usize>();
    let false_positive = 0;
    QueryOracleReport {
        status: "fixture query truth (minimum expected_contains oracle)".to_string(),
        query_count: queries.len(),
        true_positive,
        false_positive,
        false_negative,
        precision: ratio(true_positive, true_positive + false_positive),
        recall: ratio(true_positive, true_positive + false_negative),
    }
}

pub(crate) fn kotlin_oracle_limitations() -> Vec<String> {
    vec![
        "The Kotlin oracle uses the JetBrains kotlin-compiler-embeddable PSI walker for declarations only; type attribution is not required.".to_string(),
        "Symbol comparison is file/name/kind based. Generated data-class members (componentN, copy, equals, hashCode, toString), anonymous objects, locals, lambdas, the implicit `it` parameter, and parameter symbols are excluded symmetrically.".to_string(),
        "If java or the oracle jar is unavailable, the oracle is skipped while fixture query gates still run.".to_string(),
    ]
}

#[derive(Debug, Deserialize)]
pub(crate) struct KotlinOracleOutput {
    rows: Vec<[String; 3]>,
}

/// Spawn the Kotlin oracle jar against `root` and return a normalised
/// `SymbolScan`. Mirrors `javac.rs::collect_java_compiler_tree_symbol_scan`
/// in shape so the bench harness can plug either oracle in interchangeably.
pub(crate) fn collect_kotlin_compiler_tree_symbol_scan(
    root: &Path,
) -> Result<(SymbolScan, String)> {
    let exclusions = default_oracle_exclusions(root)?;
    let output = Command::new("java")
        .arg("-jar")
        .arg(KOTLIN_ORACLE_JAR)
        .arg(root)
        .output()
        .map_err(|err| SqueezyError::Graph(format!("failed to run Kotlin oracle: {err}")))?;
    if !output.status.success() {
        return Ok((
            SymbolScan::default(),
            format!(
                "Kotlin oracle unavailable: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    let parsed: KotlinOracleOutput = serde_json::from_slice(&output.stdout)
        .map_err(|err| SqueezyError::Graph(format!("invalid Kotlin oracle JSON: {err}")))?;
    let mut scan = SymbolScan::default();
    for [file, kind, name] in parsed.rows {
        scan.raw_total += 1;
        if exclusions.excludes(&file) {
            increment(&mut scan.excluded_by_kind, "ExcludedPath");
            continue;
        }
        increment_symbol(
            &mut scan.counts,
            SymbolKey {
                file,
                kind,
                name: normalize_symbol_name(&name),
            },
        );
    }
    Ok((
        scan.clone(),
        format!(
            "Kotlin oracle succeeded with {} declaration symbols",
            symbol_count(&scan.counts)
        ),
    ))
}
