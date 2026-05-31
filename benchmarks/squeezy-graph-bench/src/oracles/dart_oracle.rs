use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use serde::Deserialize;
use squeezy_core::{LanguageKind, Result, SqueezyError, SymbolKind};
use squeezy_graph::SemanticGraph;

use crate::{
    accuracy::{compare_symbol_sets, increment_symbol},
    oracles::{
        common_scan::default_oracle_exclusions, rust_analyzer::normalize_symbol_name,
    },
    report::{DartOracleReport, SymbolKey, SymbolScan},
    util::{command_exists, increment},
};

/// Path to the Dart analyzer oracle helper (relative to workspace root).
///
/// The helper lives at
/// `benchmarks/oracle-helpers/dart-oracle/bin/dart_oracle.dart` and is invoked
/// as `dart run <helper> <source-root>`. Its working directory is the helper
/// dir so `pubspec.yaml`/`.dart_tool/package_config.json` resolve correctly.
const HELPER_DIR: &str = "benchmarks/oracle-helpers/dart-oracle";
const HELPER_ENTRY: &str = "bin/dart_oracle.dart";

#[derive(Debug)]
pub(crate) struct DartOracleScan {
    pub(crate) symbols: SymbolScan,
    pub(crate) unparseable_files: Vec<String>,
    pub(crate) mode: DartOracleMode,
    pub(crate) status: String,
    pub(crate) ms: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DartOracleMode {
    Analyzer,
    ScanOnly,
}

impl DartOracleMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Analyzer => "analyzer",
            Self::ScanOnly => "scan-only",
        }
    }
}

#[derive(Debug, Deserialize)]
struct OracleLine {
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    symbols: Option<Vec<OracleSymbol>>,
    #[serde(default)]
    unparseable: Option<bool>,
    #[serde(default)]
    summary: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct OracleSymbol {
    file: String,
    kind: String,
    name: String,
}

/// Map an oracle-side kind (as emitted by `dart_oracle.dart`) to the
/// canonical bench string used for `(file, kind, name)` symbol comparison.
///
/// Dart's tree-sitter extractor emits mixins as `Trait`, extensions/
/// extension-types as `Class`, getters/setters/constructors as `Method`,
/// and enum constants as `Variant`. The oracle row labels follow the spec's
/// element-model names (`Mixin`, `Extension`, `ExtensionType`), so this
/// table flattens both sides to the same vocabulary.
fn normalize_oracle_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "Class" => Some("Class"),
        "Mixin" => Some("Trait"),
        "Extension" => Some("Class"),
        "ExtensionType" => Some("Class"),
        "Enum" => Some("Enum"),
        "Function" => Some("Function"),
        "Method" => Some("Method"),
        "Field" => Some("Field"),
        "Variable" => Some("Variable"),
        "Const" => Some("Const"),
        "Static" => Some("Static"),
        "TypeAlias" => Some("TypeAlias"),
        "Trait" => Some("Trait"),
        "Variant" => Some("Variant"),
        // `Library` rows are informational only — squeezy does not emit a
        // matching symbol (libraries are tracked via the `__dart_library__`
        // import marker), so the oracle excludes them from FP/FN accounting.
        "Library" => None,
        _ => None,
    }
}

/// Squeezy-side counterpart to `normalize_oracle_kind`: project the symbol
/// kinds Squeezy actually produces for Dart files into the same vocabulary
/// the oracle uses. Returns `None` for kinds the oracle never emits (so the
/// symbol is bucketed into `excluded_by_kind` rather than counted as an FP).
fn normalize_dart_squeezy_kind(kind: SymbolKind) -> Option<String> {
    match kind {
        SymbolKind::Class => Some("Class".to_string()),
        SymbolKind::Trait => Some("Trait".to_string()),
        SymbolKind::Enum => Some("Enum".to_string()),
        SymbolKind::Function | SymbolKind::Test => Some("Function".to_string()),
        SymbolKind::Method => Some("Method".to_string()),
        SymbolKind::Field => Some("Field".to_string()),
        SymbolKind::Variant => Some("Variant".to_string()),
        SymbolKind::Const => Some("Const".to_string()),
        SymbolKind::Static => Some("Static".to_string()),
        SymbolKind::TypeAlias => Some("TypeAlias".to_string()),
        SymbolKind::Interface
        | SymbolKind::Module
        | SymbolKind::Struct
        | SymbolKind::Union
        | SymbolKind::Impl
        | SymbolKind::Macro
        | SymbolKind::Crate
        | SymbolKind::File
        | SymbolKind::Unknown => None,
    }
}

/// Walk the squeezy graph and build a `SymbolScan` over Dart files only,
/// applying the same kind normalization as the oracle so set comparison is
/// symmetric.
fn collect_squeezy_dart_symbol_scan(
    graph: &SemanticGraph,
    excluded_files: &BTreeSet<String>,
) -> SymbolScan {
    let mut scan = SymbolScan::default();
    for symbol in graph.symbols.values() {
        let Some(file) = graph.files.get(&symbol.file_id) else {
            increment(&mut scan.excluded_by_kind, "MissingFile");
            continue;
        };
        if file.language != LanguageKind::Dart {
            continue;
        }
        scan.raw_total += 1;
        if excluded_files.contains(&file.relative_path) {
            increment(&mut scan.excluded_by_kind, "OracleUnparseableFile");
            continue;
        }
        if is_dart_codegen(&file.relative_path) {
            increment(&mut scan.excluded_by_kind, "DartCodegen");
            continue;
        }
        match normalize_dart_squeezy_kind(symbol.kind) {
            Some(kind) => {
                increment_symbol(
                    &mut scan.counts,
                    SymbolKey {
                        file: file.relative_path.clone(),
                        kind,
                        name: normalize_symbol_name(&symbol.name),
                    },
                );
            }
            None => increment(&mut scan.excluded_by_kind, &format!("{:?}", symbol.kind)),
        }
    }
    scan
}

/// Locate the Dart oracle helper directory by walking up from the supplied
/// source root looking for `benchmarks/oracle-helpers/dart-oracle/pubspec.yaml`.
///
/// The bench is run from the repo root in CI and from this workspace
/// locally; in both cases the helper sits next to the corpus, so walking up
/// at most six levels is enough to discover it without baking an absolute
/// path into the bench.
fn locate_helper_dir(root: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd);
    }
    candidates.push(root.to_path_buf());
    for base in candidates {
        let mut current: Option<&Path> = Some(base.as_path());
        let mut depth = 0;
        while let Some(dir) = current {
            let candidate = dir.join(HELPER_DIR);
            if candidate.join("pubspec.yaml").exists() {
                return Some(candidate);
            }
            if depth >= 6 {
                break;
            }
            current = dir.parent();
            depth += 1;
        }
    }
    None
}

pub(crate) fn collect_dart_oracle_scan(root: &Path) -> Result<DartOracleScan> {
    let started = Instant::now();
    if !command_exists("dart") {
        return Ok(scan_only_fallback(
            "dart binary not found on PATH; oracle degraded to scan-only".to_string(),
            started.elapsed().as_millis(),
            Vec::new(),
        ));
    }
    let Some(helper_dir) = locate_helper_dir(root) else {
        return Ok(scan_only_fallback(
            format!(
                "Dart oracle helper not found near source root {}; oracle degraded to scan-only",
                root.display()
            ),
            started.elapsed().as_millis(),
            Vec::new(),
        ));
    };
    let entry = helper_dir.join(HELPER_ENTRY);
    if !entry.exists() {
        return Ok(scan_only_fallback(
            format!(
                "Dart oracle entry script missing: {}",
                entry.display()
            ),
            started.elapsed().as_millis(),
            Vec::new(),
        ));
    }
    // Ensure the pub dependencies are fetched. `dart run` will fetch them
    // implicitly on first invocation; skipping the explicit `pub get` keeps
    // the wrapper hermetic and avoids re-resolving on every bench run.
    //
    // `current_dir` is the helper directory so `package_config.json` resolves
    // there, but the source root must be passed as an absolute path or the
    // helper will try to resolve it relative to the helper dir and fail.
    let absolute_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let output = Command::new("dart")
        .arg("run")
        .arg(entry)
        .arg(&absolute_root)
        .current_dir(&helper_dir)
        .output()
        .map_err(|err| SqueezyError::Graph(format!("failed to spawn dart oracle: {err}")))?;
    let elapsed = started.elapsed().as_millis();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let snippet = stderr
            .lines()
            .filter(|line| !line.trim().is_empty())
            .take(5)
            .collect::<Vec<_>>()
            .join(" | ");
        return Ok(scan_only_fallback(
            format!(
                "Dart analyzer oracle exited with status {}: {}; oracle degraded to scan-only",
                output.status, snippet
            ),
            elapsed,
            Vec::new(),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_oracle_stdout(&stdout, root, elapsed)
}

/// Per spec §4k/§9: `**/*.g.dart`, `**/*.freezed.dart`, `**/*.mocks.dart`
/// are codegen artifacts that are excluded from FP/FN accounting on BOTH
/// sides. The workspace crawler's `looks_generated` header sniff only
/// catches markers like "do not edit" (not "DO NOT MODIFY"), so we apply
/// the glob explicitly here.
fn is_dart_codegen(relative_path: &str) -> bool {
    const SUFFIXES: &[&str] = &[".g.dart", ".freezed.dart", ".mocks.dart"];
    SUFFIXES.iter().any(|suffix| relative_path.ends_with(suffix))
}

fn parse_oracle_stdout(stdout: &str, root: &Path, ms: u128) -> Result<DartOracleScan> {
    let exclusions = default_oracle_exclusions(root)?;
    let mut scan = SymbolScan::default();
    let mut unparseable = Vec::new();
    let mut summary_seen = false;
    let mut resolved_libraries = 0usize;
    for (lineno, line) in stdout.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed: OracleLine = serde_json::from_str(trimmed).map_err(|err| {
            SqueezyError::Graph(format!(
                "invalid Dart oracle JSON on line {lineno}: {err}; payload={trimmed}"
            ))
        })?;
        if parsed.summary.is_some() {
            summary_seen = true;
            continue;
        }
        let file = match parsed.file.as_deref() {
            Some(f) => f,
            None => continue,
        };
        if parsed.unparseable.unwrap_or(false) {
            if !exclusions.excludes(file) {
                unparseable.push(file.to_string());
            }
            continue;
        }
        resolved_libraries += 1;
        if let Some(symbols) = parsed.symbols {
            for symbol in symbols {
                scan.raw_total += 1;
                if exclusions.excludes(&symbol.file) {
                    increment(&mut scan.excluded_by_kind, "ExcludedPath");
                    continue;
                }
                if is_dart_codegen(&symbol.file) {
                    increment(&mut scan.excluded_by_kind, "DartCodegen");
                    continue;
                }
                let Some(kind) = normalize_oracle_kind(&symbol.kind) else {
                    increment(
                        &mut scan.excluded_by_kind,
                        &format!("OracleKind:{}", symbol.kind),
                    );
                    continue;
                };
                increment_symbol(
                    &mut scan.counts,
                    SymbolKey {
                        file: symbol.file,
                        kind: kind.to_string(),
                        name: normalize_symbol_name(&symbol.name),
                    },
                );
            }
        }
    }
    let mode = DartOracleMode::Analyzer;
    let status = if !summary_seen {
        format!(
            "Dart analyzer oracle emitted no summary line ({} libraries parsed)",
            resolved_libraries
        )
    } else if unparseable.is_empty() {
        format!(
            "Dart analyzer oracle succeeded ({} libraries)",
            resolved_libraries
        )
    } else {
        format!(
            "Dart analyzer oracle succeeded with {} unparseable files excluded from symbol FP accounting",
            unparseable.len()
        )
    };
    Ok(DartOracleScan {
        symbols: scan,
        unparseable_files: unparseable,
        mode,
        status,
        ms,
    })
}

fn scan_only_fallback(status: String, ms: u128, unparseable_files: Vec<String>) -> DartOracleScan {
    DartOracleScan {
        symbols: SymbolScan::default(),
        unparseable_files,
        mode: DartOracleMode::ScanOnly,
        status,
        ms,
    }
}

pub(crate) fn collect_dart_oracle_accuracy(
    root: &Path,
    graph: &SemanticGraph,
) -> Result<DartOracleReport> {
    let scan = collect_dart_oracle_scan(root)?;
    let unparseable_files = scan
        .unparseable_files
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let squeezy_symbols = match scan.mode {
        DartOracleMode::Analyzer => collect_squeezy_dart_symbol_scan(graph, &unparseable_files),
        // In scan-only mode the oracle has no symbols of its own, so
        // comparing the squeezy graph against an empty set would inflate FP
        // counts. Build an empty squeezy scan too — gates treat scan-only
        // as a "no oracle" pass.
        DartOracleMode::ScanOnly => SymbolScan::default(),
    };
    let symbols = compare_symbol_sets(&squeezy_symbols, &scan.symbols);
    let oracle_unparseable_examples = unparseable_files
        .iter()
        .take(10)
        .cloned()
        .collect::<Vec<_>>();
    let oracle_unparseable_files = unparseable_files.len();
    let mut limitations = dart_oracle_limitations();
    if scan.mode == DartOracleMode::ScanOnly {
        limitations.insert(
            0,
            "scan-only fallback: dart toolchain unavailable, oracle did not run".to_string(),
        );
    }
    Ok(DartOracleReport {
        oracle_ms: scan.ms,
        status: scan.status,
        mode: scan.mode.as_str().to_string(),
        oracle_unparseable_files,
        oracle_unparseable_examples,
        symbols,
        limitations,
    })
}

pub(crate) fn time_dart_oracle_optional(root: &Path) -> (u128, String) {
    if !command_exists("dart") {
        return (
            0,
            "skipped: dart binary not found; oracle degrades to scan-only".to_string(),
        );
    }
    match collect_dart_oracle_scan(root) {
        Ok(scan) if scan.mode == DartOracleMode::Analyzer => (scan.ms, scan.status),
        Ok(scan) => (0, scan.status),
        Err(err) => (
            0,
            format!("skipped: dart analyzer oracle failed: {err}"),
        ),
    }
}

fn dart_oracle_limitations() -> Vec<String> {
    vec![
        "The Dart oracle uses `package:analyzer`'s element model and does not exercise dart analyzer's diagnostic engine for parse-error reporting.".to_string(),
        "Codegen files (`*.g.dart`, `*.freezed.dart`, `*.mocks.dart`) are excluded from FP/FN accounting on both sides; runtime `noSuchMethod` dispatch is also excluded.".to_string(),
        "Part-of declarations are re-parented to the host library's file path so part members do not double-count; the squeezy extractor mirrors this via the `__dart_part_of__` import marker.".to_string(),
        "When the dart binary is unavailable, the oracle degrades to scan-only mode (no element-model walk); precision/recall numbers are not meaningful and gates branch on `mode == \"scan-only\"`.".to_string(),
    ]
}

#[cfg(test)]
#[path = "dart_oracle_tests.rs"]
mod tests;
