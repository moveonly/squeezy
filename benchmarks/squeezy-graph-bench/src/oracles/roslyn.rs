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
        collect_csharp_squeezy_edge_scan_excluding_files,
        collect_csharp_squeezy_symbol_scan_excluding_files, default_oracle_exclusions,
    },
    report::{
        AccuracyReport, CsharpOracleReport, DefinitionAccuracyReport, NavigationAccuracyReport,
        ReferenceAccuracyReport, SymbolKey, SymbolScan,
    },
    util::increment,
};

pub(crate) fn collect_csharp_oracle_accuracy(
    root: &Path,
    graph: &SemanticGraph,
) -> Result<CsharpOracleReport> {
    let started = Instant::now();
    let oracle = collect_csharp_oracle_symbol_scan(root)?;
    let oracle_ms = started.elapsed().as_millis();
    let unparseable_files = oracle
        .unparseable_files
        .into_iter()
        .collect::<BTreeSet<_>>();
    let squeezy_symbols =
        collect_csharp_squeezy_symbol_scan_excluding_files(graph, &unparseable_files);
    let squeezy_edges = collect_csharp_squeezy_edge_scan_excluding_files(graph, &unparseable_files);
    let mut symbols = compare_symbol_sets(&squeezy_symbols, &oracle.symbols);
    symbols.compared_kinds = csharp_compared_symbol_kinds();
    let mut edges = compare_symbol_sets(&squeezy_edges, &oracle.edges);
    edges.compared_kinds = vec!["Extends".to_string(), "Implements".to_string()];
    let oracle_unparseable_examples = unparseable_files
        .iter()
        .take(10)
        .cloned()
        .collect::<Vec<_>>();
    let oracle_unparseable_files = unparseable_files.len();

    let status_text = if oracle_unparseable_files == 0 {
        "Roslyn C# oracle succeeded".to_string()
    } else {
        format!(
            "Roslyn C# oracle succeeded with {oracle_unparseable_files} unparseable files excluded from symbol FP accounting"
        )
    };

    Ok(CsharpOracleReport {
        oracle_ms,
        oracle_build_ms: oracle.build_ms,
        status: status_text,
        oracle_unparseable_files,
        oracle_unparseable_examples,
        symbols,
        edges,
        limitations: vec![
            "The C# oracle uses Roslyn's CSharpSyntaxTree (syntactic, not semantic), so it counts declarations but does not resolve members inherited from referenced assemblies.".to_string(),
            "C# edge accuracy currently compares syntactic extends/implements edges; overload, dynamic dispatch, extension methods, and accessor flow remain query-spec coverage rather than oracle coverage.".to_string(),
            "Symbol comparison is file/name/kind based; the oracle reports partial declarations once per source file, mirroring squeezy's own behavior.".to_string(),
            "C# files that Roslyn cannot parse (e.g. invalid syntax) are reported as oracle_unparseable and excluded from Squeezy false-positive accounting.".to_string(),
        ],
    })
}

pub(crate) fn csharp_oracle_to_accuracy(report: &CsharpOracleReport) -> AccuracyReport {
    let mut symbols = report.symbols.clone();
    symbols.compared_kinds = csharp_compared_symbol_kinds();
    AccuracyReport {
        rust_analyzer_symbols_ms: Some(report.oracle_ms),
        rust_analyzer_symbol_status: report.status.clone(),
        symbols,
        navigation: NavigationAccuracyReport {
            rust_analyzer_lsp_ms: None,
            rust_analyzer_lsp_status: "C# LSP navigation oracle not used".to_string(),
            requested_probe_limit: 0,
            definitions: DefinitionAccuracyReport::default(),
            references: ReferenceAccuracyReport::default(),
            limitations: vec![
                "C# accuracy currently compares symbol declarations against Roslyn; LSP-style go-to-definition probes are not exercised yet.".to_string(),
            ],
        },
        limitations: report.limitations.clone(),
    }
}

fn csharp_compared_symbol_kinds() -> Vec<String> {
    vec![
        "Class".to_string(),
        "Interface".to_string(),
        "Module".to_string(),
        "Struct".to_string(),
        "Enum".to_string(),
        "Function".to_string(),
        "Method".to_string(),
        "TypeAlias".to_string(),
        "Field".to_string(),
        "Variant".to_string(),
    ]
}

#[derive(Debug, Deserialize)]
pub(crate) struct CsharpOracleOutput {
    pub(crate) rows: Vec<[String; 3]>,
    #[serde(default)]
    pub(crate) edges: Vec<[String; 3]>,
    pub(crate) unparseable_files: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct CsharpOracleSymbolScan {
    pub(crate) symbols: SymbolScan,
    pub(crate) edges: SymbolScan,
    pub(crate) unparseable_files: Vec<String>,
    pub(crate) build_ms: Option<u128>,
}

pub(crate) fn collect_csharp_oracle_symbol_scan(root: &Path) -> Result<CsharpOracleSymbolScan> {
    let (dll, build_ms) = ensure_csharp_oracle_built()?;
    let exclusions = default_oracle_exclusions(root)?;
    let output = Command::new("dotnet")
        .arg(&dll)
        .arg(root)
        .output()
        .map_err(|err| SqueezyError::Graph(format!("failed to run Roslyn C# oracle: {err}")))?;
    if !output.status.success() {
        return Err(SqueezyError::Graph(format!(
            "Roslyn C# oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let output: CsharpOracleOutput = serde_json::from_slice(&output.stdout)
        .map_err(|err| SqueezyError::Graph(format!("invalid Roslyn C# oracle JSON: {err}")))?;
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
    Ok(CsharpOracleSymbolScan {
        symbols: scan,
        edges,
        unparseable_files,
        build_ms,
    })
}

pub(crate) fn ensure_csharp_oracle_built() -> Result<(PathBuf, Option<u128>)> {
    let project = csharp_oracle_project_dir()?;
    let dll = project
        .join("bin")
        .join("Release")
        .join("net8.0")
        .join("CsharpOracle.dll");
    if dll.exists() && csharp_oracle_is_fresh(&project, &dll) {
        return Ok((dll, None));
    }
    let started = Instant::now();
    let status = Command::new("dotnet")
        .arg("build")
        .arg(&project)
        .arg("-c")
        .arg("Release")
        .arg("--nologo")
        .arg("-v")
        .arg("minimal")
        .status()
        .map_err(|err| SqueezyError::Graph(format!("failed to build Roslyn C# oracle: {err}")))?;
    let build_ms = started.elapsed().as_millis();
    if !status.success() {
        return Err(SqueezyError::Graph(format!(
            "Roslyn C# oracle build failed with {status}"
        )));
    }
    if !dll.exists() {
        return Err(SqueezyError::Graph(format!(
            "Roslyn C# oracle build did not produce {}",
            dll.display()
        )));
    }
    Ok((dll, Some(build_ms)))
}

fn csharp_oracle_is_fresh(project: &Path, dll: &Path) -> bool {
    let Ok(dll_modified) = dll.metadata().and_then(|metadata| metadata.modified()) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(project) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let extension = path.extension().and_then(|extension| extension.to_str());
        if !matches!(extension, Some("cs") | Some("csproj")) {
            continue;
        }
        let Ok(modified) = path.metadata().and_then(|metadata| metadata.modified()) else {
            return false;
        };
        if modified > dll_modified {
            return false;
        }
    }
    true
}

pub(crate) fn csharp_oracle_project_dir() -> Result<PathBuf> {
    if let Ok(value) = std::env::var("SQUEEZY_CSHARP_ORACLE_DIR")
        && !value.is_empty()
    {
        let path = PathBuf::from(value);
        if path.exists() {
            return Ok(path);
        }
    }
    let candidates: [PathBuf; 3] = [
        PathBuf::from("benchmarks/oracle/csharp"),
        PathBuf::from("../oracle/csharp"),
        PathBuf::from("../../benchmarks/oracle/csharp"),
    ];
    for candidate in candidates {
        if candidate.join("CsharpOracle.csproj").exists() {
            return Ok(candidate);
        }
    }
    Err(SqueezyError::Graph(
        "could not locate benchmarks/oracle/csharp; set SQUEEZY_CSHARP_ORACLE_DIR".to_string(),
    ))
}

#[cfg(test)]
#[path = "roslyn_tests.rs"]
mod tests;
