use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use serde::Deserialize;
use squeezy_core::{Result, SqueezyError};
use squeezy_graph::SemanticGraph;

use crate::{
    accuracy::{compare_symbol_sets, increment_symbol},
    oracles::common_scan::{
        collect_php_squeezy_edge_scan_excluding_files,
        collect_php_squeezy_symbol_scan_excluding_files, default_oracle_exclusions,
    },
    report::{PhpOracleReport, QueryOracleReport, QueryReport, SymbolKey, SymbolScan},
    util::{command_exists, increment},
};

pub(crate) fn time_php_oracle_optional(root: &Path) -> (u128, String) {
    if !command_exists("php") {
        return (0, "skipped: php not found".to_string());
    }
    let Some(helper) = locate_php_oracle_helper() else {
        return (0, "skipped: php oracle helper not found".to_string());
    };
    let vendor = helper.parent().map(|dir| dir.join("vendor"));
    if !vendor.as_ref().map(|dir| dir.exists()).unwrap_or(false) {
        return (
            0,
            "skipped: composer install not run in benchmarks/oracle-helpers/php-oracle".to_string(),
        );
    }
    let started = Instant::now();
    match run_php_oracle(&helper, root) {
        Ok(_) => (started.elapsed().as_millis(), "php oracle succeeded".to_string()),
        Err(err) => (0, format!("skipped: php oracle failed: {err}")),
    }
}

pub(crate) fn collect_php_oracle_accuracy(
    root: &Path,
    graph: &SemanticGraph,
    queries: &[QueryReport],
) -> Result<PhpOracleReport> {
    if !command_exists("php") {
        return Ok(skipped_php_oracle_report(
            graph,
            queries,
            "skipped: php not found",
        ));
    }
    let Some(helper) = locate_php_oracle_helper() else {
        return Ok(skipped_php_oracle_report(
            graph,
            queries,
            "skipped: php oracle helper not found",
        ));
    };
    let vendor = helper.parent().map(|dir| dir.join("vendor"));
    if !vendor.as_ref().map(|dir| dir.exists()).unwrap_or(false) {
        return Ok(skipped_php_oracle_report(
            graph,
            queries,
            "skipped: composer install not run in benchmarks/oracle-helpers/php-oracle",
        ));
    }
    let started = Instant::now();
    let scan = match run_php_oracle(&helper, root) {
        Ok(scan) => scan,
        Err(err) => {
            return Ok(skipped_php_oracle_report(
                graph,
                queries,
                &format!("skipped: php oracle failed: {err}"),
            ));
        }
    };
    let oracle_ms = started.elapsed().as_millis();
    let unparseable_files = scan
        .unparseable_files
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let squeezy_symbols =
        collect_php_squeezy_symbol_scan_excluding_files(graph, &unparseable_files);
    let squeezy_edges = collect_php_squeezy_edge_scan_excluding_files(graph, &unparseable_files);
    let mut symbols = compare_symbol_sets(&squeezy_symbols, &scan.symbols);
    symbols.compared_kinds = php_compared_symbol_kinds();
    let mut edges = compare_symbol_sets(&squeezy_edges, &scan.edges);
    edges.compared_kinds = vec!["Extends".to_string(), "Implements".to_string()];
    let oracle_unparseable_examples = unparseable_files.iter().take(10).cloned().collect();
    let oracle_unparseable_files = unparseable_files.len();
    let status_text = if oracle_unparseable_files == 0 {
        "nikic/PHP-Parser oracle succeeded".to_string()
    } else {
        format!(
            "nikic/PHP-Parser oracle succeeded with {oracle_unparseable_files} unparseable files excluded from symbol FP accounting"
        )
    };

    Ok(PhpOracleReport {
        oracle_ms: Some(oracle_ms),
        status: status_text,
        oracle_unparseable_files,
        oracle_unparseable_examples,
        symbols,
        edges,
        navigation: collect_query_oracle_accuracy(queries),
        limitations: php_oracle_limitations(),
    })
}

fn skipped_php_oracle_report(
    graph: &SemanticGraph,
    queries: &[QueryReport],
    status: &str,
) -> PhpOracleReport {
    let empty = BTreeSet::new();
    let mut symbols = compare_symbol_sets(
        &collect_php_squeezy_symbol_scan_excluding_files(graph, &empty),
        &SymbolScan::default(),
    );
    symbols.compared_kinds = php_compared_symbol_kinds();
    let mut edges = compare_symbol_sets(
        &collect_php_squeezy_edge_scan_excluding_files(graph, &empty),
        &SymbolScan::default(),
    );
    edges.compared_kinds = vec!["Extends".to_string(), "Implements".to_string()];
    PhpOracleReport {
        oracle_ms: None,
        status: status.to_string(),
        oracle_unparseable_files: 0,
        oracle_unparseable_examples: Vec::new(),
        symbols,
        edges,
        navigation: collect_query_oracle_accuracy(queries),
        limitations: php_oracle_limitations(),
    }
}

fn php_compared_symbol_kinds() -> Vec<String> {
    vec![
        "Class".to_string(),
        "Interface".to_string(),
        "Trait".to_string(),
        "Enum".to_string(),
        "Namespace".to_string(),
        "Function".to_string(),
        "Method".to_string(),
        "Property".to_string(),
        "Constant".to_string(),
        "Variant".to_string(),
    ]
}

fn php_oracle_limitations() -> Vec<String> {
    vec![
        "The PHP oracle uses nikic/PHP-Parser declaration walks; it counts declarations only and does not attempt type resolution.".to_string(),
        "Magic method dispatch, dynamic class names, eval, heredoc bodies, and anonymous classes remain heuristic on the squeezy side and are intentionally not modelled by the oracle.".to_string(),
        "PHP files that nikic/PHP-Parser cannot parse (syntax errors, encoding issues) are reported as oracle_unparseable and excluded from squeezy false-positive accounting.".to_string(),
        "Symbol comparison is file/name/kind based; overload, late-static-binding, and trait conflict resolution are not modelled in v1.".to_string(),
    ]
}

fn collect_query_oracle_accuracy(queries: &[QueryReport]) -> QueryOracleReport {
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
        precision: crate::accuracy::ratio(true_positive, true_positive + false_positive),
        recall: crate::accuracy::ratio(true_positive, true_positive + false_negative),
    }
}

#[derive(Debug, Deserialize)]
struct PhpOracleOutput {
    rows: Vec<[String; 3]>,
    #[serde(default)]
    edges: Vec<[String; 3]>,
    #[serde(default)]
    unparseable_files: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct PhpOracleSymbolScan {
    pub(crate) symbols: SymbolScan,
    pub(crate) edges: SymbolScan,
    pub(crate) unparseable_files: Vec<String>,
}

pub(crate) fn run_php_oracle(helper: &Path, root: &Path) -> Result<PhpOracleSymbolScan> {
    let exclusions = default_oracle_exclusions(root)?;
    let output = Command::new("php")
        .arg(helper)
        .arg(root)
        .output()
        .map_err(|err| SqueezyError::Graph(format!("failed to run PHP oracle: {err}")))?;
    if !output.status.success() {
        return Err(SqueezyError::Graph(format!(
            "PHP oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let output: PhpOracleOutput = serde_json::from_slice(&output.stdout)
        .map_err(|err| SqueezyError::Graph(format!("invalid PHP oracle JSON: {err}")))?;
    let mut scan = SymbolScan::default();
    for [file, kind, name] in output.rows {
        scan.raw_total += 1;
        if exclusions.excludes(&file) {
            increment(&mut scan.excluded_by_kind, "ExcludedPath");
            continue;
        }
        increment_symbol(&mut scan.counts, SymbolKey { file, kind, name });
    }
    let mut edges = SymbolScan::default();
    for [file, kind, name] in output.edges {
        edges.raw_total += 1;
        if exclusions.excludes(&file) {
            increment(&mut edges.excluded_by_kind, "ExcludedPath");
            continue;
        }
        increment_symbol(&mut edges.counts, SymbolKey { file, kind, name });
    }
    let unparseable_files = output
        .unparseable_files
        .into_iter()
        .filter(|file| !exclusions.excludes(file))
        .collect();
    Ok(PhpOracleSymbolScan {
        symbols: scan,
        edges,
        unparseable_files,
    })
}

pub(crate) fn locate_php_oracle_helper() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("SQUEEZY_PHP_ORACLE_HELPER")
        && !value.is_empty()
    {
        let path = PathBuf::from(value);
        if path.exists() {
            return Some(path);
        }
    }
    [
        PathBuf::from("benchmarks/oracle-helpers/php-oracle/oracle.php"),
        PathBuf::from("../oracle-helpers/php-oracle/oracle.php"),
        PathBuf::from("../../benchmarks/oracle-helpers/php-oracle/oracle.php"),
    ]
    .into_iter()
    .find(|candidate| candidate.exists())
}

#[cfg(test)]
#[path = "php_oracle_tests.rs"]
mod tests;
