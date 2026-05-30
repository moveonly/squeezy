use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use squeezy_core::SymbolId;

#[derive(Debug, Deserialize)]
pub(crate) struct QuerySpecFile {
    pub(crate) queries: Vec<QuerySpec>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct QuerySpec {
    pub(crate) id: String,
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) text: String,
    pub(crate) symbol_kind: Option<String>,
    pub(crate) owner_kind: Option<String>,
    pub(crate) attribute: Option<String>,
    pub(crate) from: Option<String>,
    pub(crate) to: Option<String>,
    pub(crate) expected_contains: Vec<String>,
    #[serde(default)]
    pub(crate) documented_misses: Vec<DocumentedMiss>,
    #[serde(default)]
    pub(crate) baseline: Option<GrepBaselineSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct DocumentedMiss {
    pub(crate) result: String,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct GrepBaselineSpec {
    #[serde(default)]
    pub(crate) pattern: Option<String>,
    #[serde(default)]
    pub(crate) include: Vec<String>,
    #[serde(default)]
    pub(crate) mode: GrepBaselineMode,
    #[serde(default)]
    pub(crate) unsupported_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GrepBaselineMode {
    Paths,
    Count,
    FirstLine,
    Unsupported,
}

impl Default for GrepBaselineMode {
    fn default() -> Self {
        Self::Paths
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct BenchmarkReport {
    pub(crate) corpus_case: Option<CorpusCaseReport>,
    pub(crate) language: String,
    pub(crate) fixture: String,
    pub(crate) spec: String,
    pub(crate) validation_ms: u128,
    pub(crate) validation_status: String,
    pub(crate) squeezy_build_ms: u128,
    pub(crate) squeezy_query_ms: u128,
    pub(crate) squeezy_total_ms: u128,
    pub(crate) build_phases: BuildPhaseReport,
    pub(crate) faster_than_validation: bool,
    pub(crate) tool_metrics: ToolMetricsReport,
    pub(crate) answer_quality: AnswerQualityReport,
    pub(crate) fallback_quality: FallbackQualityReport,
    pub(crate) graph: GraphReport,
    pub(crate) accuracy: AccuracyReport,
    pub(crate) python_oracle: Option<PythonOracleReport>,
    pub(crate) js_ts_oracle: Option<JsTsOracleReport>,
    pub(crate) java_oracle: Option<JavaOracleReport>,
    pub(crate) csharp_oracle: Option<CsharpOracleReport>,
    pub(crate) go_oracle: Option<GoOracleReport>,
    pub(crate) ruby_oracle: Option<RubyOracleReport>,
    pub(crate) refresh_probe: Option<RefreshProbeReport>,
    pub(crate) heuristic_iterations: Vec<HeuristicIterationReport>,
    pub(crate) queries: Vec<QueryReport>,
    pub(crate) mixed_workload: Option<MixedWorkloadReport>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CorpusCaseReport {
    pub(crate) name: String,
    pub(crate) family: String,
    pub(crate) tier: String,
    pub(crate) source_url: Option<String>,
    pub(crate) source_ref: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ToolMetricsReport {
    pub(crate) graph_queries: usize,
    pub(crate) grep_baseline_queries: usize,
    pub(crate) mixed_scenarios: usize,
    pub(crate) deterministic_tool_calls: usize,
    pub(crate) wall_ms: u128,
    pub(crate) estimated_usd_micros: u64,
    pub(crate) cost_basis: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct AnswerQualityReport {
    pub(crate) query_count: usize,
    pub(crate) expected_checks: usize,
    pub(crate) satisfied_checks: usize,
    pub(crate) missing_checks: usize,
    pub(crate) extra_results: usize,
    pub(crate) documented_misses: usize,
    pub(crate) passed: bool,
    pub(crate) oracle_status: String,
    pub(crate) oracle_precision: Option<f64>,
    pub(crate) oracle_recall: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct FallbackQualityReport {
    pub(crate) unsupported_files: usize,
    pub(crate) unsupported_file_samples: Vec<String>,
    pub(crate) excluded_files: usize,
    pub(crate) excluded_dirs: usize,
    pub(crate) excluded_bytes: u64,
    pub(crate) coverage_reasons: BTreeMap<String, FallbackReasonReport>,
    pub(crate) edge_confidence: BTreeMap<String, usize>,
    pub(crate) low_confidence_edges: usize,
    pub(crate) fallback_rate: f64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct FallbackReasonReport {
    pub(crate) files: usize,
    pub(crate) dirs: usize,
    pub(crate) bytes: u64,
    pub(crate) samples: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct GraphReport {
    pub(crate) files: usize,
    pub(crate) symbols: usize,
    pub(crate) edges: usize,
    pub(crate) body_hits: usize,
    pub(crate) references: usize,
    pub(crate) calls: usize,
    pub(crate) body_hit_trigram_indexed: bool,
    pub(crate) body_hit_trigram_terms: usize,
    pub(crate) reference_index_terms: usize,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct BuildPhaseReport {
    pub(crate) crawl_ms: u128,
    pub(crate) parse_ms: u128,
    pub(crate) declaration_graph_ms: u128,
    pub(crate) full_graph_ms: u128,
    pub(crate) total_ms: u128,
}

#[derive(Debug, Serialize)]
pub(crate) struct QueryReport {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) expected_contains: Vec<String>,
    pub(crate) actual: Vec<String>,
    pub(crate) missing: Vec<String>,
    pub(crate) extras: Vec<String>,
    pub(crate) documented_misses: Vec<DocumentedMiss>,
    pub(crate) baseline: QueryBaselineReport,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct QueryBaselineReport {
    pub(crate) status: QueryBaselineStatus,
    pub(crate) status_detail: String,
    pub(crate) pattern: Option<String>,
    pub(crate) include: Vec<String>,
    pub(crate) files_scanned: usize,
    pub(crate) bytes_read: u64,
    pub(crate) matches_returned: usize,
    pub(crate) actual: Vec<String>,
    pub(crate) semantic_relation_supported: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QueryBaselineStatus {
    Ran,
    Skipped,
    Unsupported,
}

#[derive(Debug, Serialize)]
pub(crate) struct MixedWorkloadReport {
    pub(crate) repo: String,
    pub(crate) requested_scenarios: usize,
    pub(crate) available_scenarios: usize,
    pub(crate) executed_scenarios: usize,
    pub(crate) tools: Vec<String>,
    pub(crate) compiler_check_ms: Option<u128>,
    pub(crate) compiler_check_status: String,
    pub(crate) rust_analyzer_ms: Option<u128>,
    pub(crate) rust_analyzer_status: String,
    pub(crate) squeezy_build_ms: u128,
    pub(crate) squeezy_query_ms: u128,
    pub(crate) squeezy_total_ms: u128,
    pub(crate) faster_than_compiler_check: Option<bool>,
    pub(crate) faster_than_rust_analyzer: Option<bool>,
    pub(crate) query_counts: BTreeMap<String, usize>,
    pub(crate) query_time_ms: BTreeMap<String, u128>,
    pub(crate) refresh_probe: RefreshProbeReport,
    pub(crate) accuracy: AccuracyReport,
}

#[derive(Debug, Serialize)]
pub(crate) struct RefreshProbeReport {
    pub(crate) language: String,
    pub(crate) copied_source_files: usize,
    pub(crate) edited_files: usize,
    pub(crate) refresh_ms: u128,
    pub(crate) reparsed_files: usize,
    pub(crate) changed_files: usize,
    pub(crate) changed_paths_from_events: usize,
    pub(crate) changed_paths_from_polling: usize,
    pub(crate) unchanged_event_paths: usize,
    pub(crate) budget_exhausted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AccuracyReport {
    pub(crate) rust_analyzer_symbols_ms: Option<u128>,
    pub(crate) rust_analyzer_symbol_status: String,
    pub(crate) symbols: AccuracySetReport,
    pub(crate) navigation: NavigationAccuracyReport,
    pub(crate) limitations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AccuracySetReport {
    pub(crate) compared_kinds: Vec<String>,
    pub(crate) rust_analyzer_raw_total: usize,
    pub(crate) rust_analyzer_total: usize,
    pub(crate) rust_analyzer_unique: usize,
    pub(crate) rust_analyzer_excluded_by_kind: BTreeMap<String, usize>,
    pub(crate) rust_analyzer_skipped_non_utf8_files: usize,
    pub(crate) squeezy_raw_total: usize,
    pub(crate) squeezy_total: usize,
    pub(crate) squeezy_unique: usize,
    pub(crate) squeezy_excluded_by_kind: BTreeMap<String, usize>,
    pub(crate) true_positive: usize,
    pub(crate) false_positive: usize,
    pub(crate) false_negative: usize,
    pub(crate) precision: f64,
    pub(crate) recall: f64,
    pub(crate) false_positive_examples: Vec<String>,
    pub(crate) false_negative_examples: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct PythonOracleReport {
    pub(crate) oracle_ms: u128,
    pub(crate) status: String,
    pub(crate) oracle_unparseable_files: usize,
    pub(crate) oracle_unparseable_examples: Vec<String>,
    pub(crate) symbols: AccuracySetReport,
    pub(crate) limitations: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct JsTsOracleReport {
    pub(crate) oracle_ms: u128,
    pub(crate) status: String,
    pub(crate) symbols: AccuracySetReport,
    pub(crate) limitations: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct JavaOracleReport {
    pub(crate) oracle_ms: Option<u128>,
    pub(crate) status: String,
    pub(crate) symbols: AccuracySetReport,
    pub(crate) navigation: QueryOracleReport,
    pub(crate) limitations: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct QueryOracleReport {
    pub(crate) status: String,
    pub(crate) query_count: usize,
    pub(crate) true_positive: usize,
    pub(crate) false_positive: usize,
    pub(crate) false_negative: usize,
    pub(crate) precision: f64,
    pub(crate) recall: f64,
}

#[derive(Debug, Serialize)]
pub(crate) struct CsharpOracleReport {
    pub(crate) oracle_ms: u128,
    pub(crate) oracle_build_ms: Option<u128>,
    pub(crate) status: String,
    pub(crate) oracle_unparseable_files: usize,
    pub(crate) oracle_unparseable_examples: Vec<String>,
    pub(crate) symbols: AccuracySetReport,
    pub(crate) edges: AccuracySetReport,
    pub(crate) limitations: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct GoOracleReport {
    pub(crate) oracle_ms: u128,
    pub(crate) status: String,
    pub(crate) oracle_unparseable_files: usize,
    pub(crate) oracle_unparseable_examples: Vec<String>,
    pub(crate) symbols: AccuracySetReport,
    pub(crate) limitations: Vec<String>,
}

/// Ruby Prism oracle (see `docs/internal/lang-specs/ruby.md` §9). When the
/// Ruby toolchain is missing the oracle degrades to a self-compare scan and
/// `mode` records `"scan-only"`.
#[derive(Debug, Serialize)]
pub(crate) struct RubyOracleReport {
    pub(crate) oracle_ms: u128,
    pub(crate) status: String,
    pub(crate) mode: String,
    pub(crate) oracle_unparseable_files: usize,
    pub(crate) oracle_unparseable_examples: Vec<String>,
    pub(crate) symbols: AccuracySetReport,
    pub(crate) limitations: Vec<String>,
}

/// Per-iteration heuristic notes for the Go benchmark.
///
/// Each entry documents a heuristic decision (accepted, rejected, or targeted
/// next) and the reason. FP/FN deltas are intentionally not stored here: they
/// would need historical snapshots to be meaningful, and a single benchmark
/// run only knows the current state. The current state is already reported in
/// `go_oracle.symbols`; consumers that need before/after numbers should diff
/// JSON reports across runs.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct HeuristicIterationReport {
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) notes: Vec<String>,
    /// Optional before/after precision/recall snapshots taken at the moment
    /// the iteration entry is recorded. Spec §10 (Ruby) calls for these so
    /// reports can show the delta a heuristic produced without diffing two
    /// separate runs. All four are `None` for entries that pre-date the
    /// delta-tracking work, including the existing Go iterations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) baseline_precision: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) baseline_recall: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) new_precision: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) new_recall: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SymbolScan {
    pub(crate) counts: BTreeMap<SymbolKey, usize>,
    pub(crate) raw_total: usize,
    pub(crate) excluded_by_kind: BTreeMap<String, usize>,
    pub(crate) skipped_non_utf8_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SymbolKey {
    pub(crate) file: String,
    pub(crate) kind: String,
    pub(crate) name: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct NavigationAccuracyReport {
    pub(crate) rust_analyzer_lsp_ms: Option<u128>,
    pub(crate) rust_analyzer_lsp_status: String,
    pub(crate) requested_probe_limit: usize,
    pub(crate) definitions: DefinitionAccuracyReport,
    pub(crate) references: ReferenceAccuracyReport,
    pub(crate) limitations: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct DefinitionAccuracyReport {
    pub(crate) available_probes: usize,
    pub(crate) probes: usize,
    pub(crate) true_positive: usize,
    pub(crate) false_positive: usize,
    pub(crate) false_negative: usize,
    pub(crate) unresolved_agreement: usize,
    pub(crate) squeezy_only: usize,
    pub(crate) wrong_target: usize,
    pub(crate) precision: f64,
    pub(crate) recall: f64,
    pub(crate) examples: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct ReferenceAccuracyReport {
    pub(crate) available_symbols: usize,
    pub(crate) symbols_sampled: usize,
    pub(crate) true_positive: usize,
    pub(crate) false_positive: usize,
    pub(crate) false_negative: usize,
    pub(crate) precision: f64,
    pub(crate) recall: f64,
    pub(crate) false_positive_examples: Vec<String>,
    pub(crate) false_negative_examples: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct DefinitionProbe {
    pub(crate) label: String,
    pub(crate) uri: String,
    pub(crate) path: PathBuf,
    pub(crate) position: LspPosition,
    pub(crate) squeezy_target: Option<SymbolId>,
}

#[derive(Debug, Clone)]
pub(crate) struct ReferenceProbe {
    pub(crate) label: String,
    pub(crate) uri: String,
    pub(crate) path: PathBuf,
    pub(crate) position: LspPosition,
    pub(crate) symbol_id: SymbolId,
    pub(crate) name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct LspPosition {
    pub(crate) line: u32,
    pub(crate) character: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct LocationKey {
    pub(crate) file: String,
    pub(crate) line: u32,
    pub(crate) character: u32,
}

impl LocationKey {
    pub(crate) fn render(&self) -> String {
        format!("{}:{}:{}", self.file, self.line + 1, self.character + 1)
    }
}

impl SymbolKey {
    pub(crate) fn render(&self) -> String {
        format!("{}:{}:{}", self.file, self.kind, self.name)
    }
}
