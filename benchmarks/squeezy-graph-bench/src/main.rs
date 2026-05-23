use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use squeezy_core::{EdgeKind, LanguageKind, Result, SqueezyError, SymbolId, SymbolKind};
use squeezy_graph::{BodySearchQuery, GraphManager, RefreshConfig, SemanticGraph, SignatureQuery};
use squeezy_parse::{BodyHitKind, RustParser};
use squeezy_workspace::{CrawlOptions, WorkspaceCrawler};

#[derive(Debug, Deserialize)]
struct QuerySpecFile {
    queries: Vec<QuerySpec>,
}

#[derive(Debug, Deserialize)]
struct QuerySpec {
    id: String,
    kind: String,
    #[serde(default)]
    text: String,
    symbol_kind: Option<String>,
    owner_kind: Option<String>,
    attribute: Option<String>,
    from: Option<String>,
    to: Option<String>,
    expected_contains: Vec<String>,
    #[serde(default)]
    documented_misses: Vec<DocumentedMiss>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DocumentedMiss {
    result: String,
    reason: String,
}

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    language: String,
    fixture: String,
    spec: String,
    rustc_check_ms: u128,
    validation_ms: u128,
    validation_status: String,
    squeezy_build_ms: u128,
    squeezy_query_ms: u128,
    squeezy_total_ms: u128,
    faster_than_rustc_check: bool,
    faster_than_validation: bool,
    graph: GraphReport,
    accuracy: AccuracyReport,
    python_oracle: Option<PythonOracleReport>,
    java_oracle: Option<JavaOracleReport>,
    queries: Vec<QueryReport>,
    mixed_workload: Option<MixedWorkloadReport>,
}

#[derive(Debug, Serialize)]
struct GraphReport {
    files: usize,
    symbols: usize,
    edges: usize,
    body_hits: usize,
    references: usize,
    calls: usize,
}

#[derive(Debug, Serialize)]
struct QueryReport {
    id: String,
    kind: String,
    expected_contains: Vec<String>,
    actual: Vec<String>,
    missing: Vec<String>,
    extras: Vec<String>,
    documented_misses: Vec<DocumentedMiss>,
}

#[derive(Debug, Serialize)]
struct MixedWorkloadReport {
    repo: String,
    requested_scenarios: usize,
    available_scenarios: usize,
    executed_scenarios: usize,
    tools: Vec<String>,
    compiler_check_ms: Option<u128>,
    compiler_check_status: String,
    rust_analyzer_ms: Option<u128>,
    rust_analyzer_status: String,
    squeezy_build_ms: u128,
    squeezy_query_ms: u128,
    squeezy_total_ms: u128,
    faster_than_compiler_check: Option<bool>,
    faster_than_rust_analyzer: Option<bool>,
    query_counts: BTreeMap<String, usize>,
    query_time_ms: BTreeMap<String, u128>,
    refresh_probe: RefreshProbeReport,
    accuracy: AccuracyReport,
}

#[derive(Debug, Serialize)]
struct RefreshProbeReport {
    copied_rust_files: usize,
    edited_files: usize,
    refresh_ms: u128,
    reparsed_files: usize,
    changed_files: usize,
    budget_exhausted: bool,
}

#[derive(Debug, Clone, Serialize)]
struct AccuracyReport {
    rust_analyzer_symbols_ms: Option<u128>,
    rust_analyzer_symbol_status: String,
    symbols: AccuracySetReport,
    navigation: NavigationAccuracyReport,
    limitations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct AccuracySetReport {
    compared_kinds: Vec<String>,
    rust_analyzer_raw_total: usize,
    rust_analyzer_total: usize,
    rust_analyzer_unique: usize,
    rust_analyzer_excluded_by_kind: BTreeMap<String, usize>,
    rust_analyzer_skipped_non_utf8_files: usize,
    squeezy_raw_total: usize,
    squeezy_total: usize,
    squeezy_unique: usize,
    squeezy_excluded_by_kind: BTreeMap<String, usize>,
    true_positive: usize,
    false_positive: usize,
    false_negative: usize,
    precision: f64,
    recall: f64,
    false_positive_examples: Vec<String>,
    false_negative_examples: Vec<String>,
}

#[derive(Debug, Serialize)]
struct PythonOracleReport {
    oracle_ms: u128,
    status: String,
    oracle_unparseable_files: usize,
    oracle_unparseable_examples: Vec<String>,
    symbols: AccuracySetReport,
    limitations: Vec<String>,
}

#[derive(Debug, Serialize)]
struct JavaOracleReport {
    oracle_ms: Option<u128>,
    status: String,
    symbols: AccuracySetReport,
    navigation: QueryOracleReport,
    limitations: Vec<String>,
}

#[derive(Debug, Serialize)]
struct QueryOracleReport {
    status: String,
    query_count: usize,
    true_positive: usize,
    false_positive: usize,
    false_negative: usize,
    precision: f64,
    recall: f64,
}

#[derive(Debug, Clone, Default)]
struct SymbolScan {
    counts: BTreeMap<SymbolKey, usize>,
    raw_total: usize,
    excluded_by_kind: BTreeMap<String, usize>,
    skipped_non_utf8_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SymbolKey {
    file: String,
    kind: String,
    name: String,
}

#[derive(Debug, Clone, Serialize)]
struct NavigationAccuracyReport {
    rust_analyzer_lsp_ms: Option<u128>,
    rust_analyzer_lsp_status: String,
    requested_probe_limit: usize,
    definitions: DefinitionAccuracyReport,
    references: ReferenceAccuracyReport,
    limitations: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct DefinitionAccuracyReport {
    available_probes: usize,
    probes: usize,
    true_positive: usize,
    false_positive: usize,
    false_negative: usize,
    unresolved_agreement: usize,
    squeezy_only: usize,
    wrong_target: usize,
    precision: f64,
    recall: f64,
    examples: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct ReferenceAccuracyReport {
    available_symbols: usize,
    symbols_sampled: usize,
    true_positive: usize,
    false_positive: usize,
    false_negative: usize,
    precision: f64,
    recall: f64,
    false_positive_examples: Vec<String>,
    false_negative_examples: Vec<String>,
}

#[derive(Debug, Clone)]
struct DefinitionProbe {
    label: String,
    uri: String,
    path: PathBuf,
    position: LspPosition,
    squeezy_target: Option<SymbolId>,
}

#[derive(Debug, Clone)]
struct ReferenceProbe {
    label: String,
    uri: String,
    path: PathBuf,
    position: LspPosition,
    symbol_id: SymbolId,
    name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct LspPosition {
    line: u32,
    character: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LocationKey {
    file: String,
    line: u32,
    character: u32,
}

impl LocationKey {
    fn render(&self) -> String {
        format!("{}:{}:{}", self.file, self.line + 1, self.character + 1)
    }
}

impl SymbolKey {
    fn render(&self) -> String {
        format!("{}:{}:{}", self.file, self.kind, self.name)
    }
}

fn main() -> Result<()> {
    let args = Args::parse()?;
    let spec_text = fs::read_to_string(&args.spec)?;
    let spec: QuerySpecFile = serde_json::from_str(&spec_text)
        .map_err(|err| SqueezyError::Graph(format!("invalid benchmark spec: {err}")))?;

    let (validation_ms, validation_status) = match args.language {
        BenchmarkLanguage::Java => time_java_oracle_optional(&args.fixture),
        BenchmarkLanguage::Rust => (time_cargo_check(&args.fixture)?, "cargo check".to_string()),
        BenchmarkLanguage::Python => (
            time_python_ast_oracle(&args.fixture)?,
            "CPython ast oracle".to_string(),
        ),
    };

    let build_started = Instant::now();
    let graph = build_graph(&args.fixture)?;
    let squeezy_build_ms = build_started.elapsed().as_millis();

    let query_started = Instant::now();
    let query_reports = spec
        .queries
        .iter()
        .map(|query| run_query(&graph, query))
        .collect::<Result<Vec<_>>>()?;
    let squeezy_query_ms = query_started.elapsed().as_millis();
    let squeezy_total_ms = squeezy_build_ms + squeezy_query_ms;
    let accuracy = match args.language {
        BenchmarkLanguage::Java => empty_accuracy("rust-analyzer oracle not used for Java"),
        BenchmarkLanguage::Rust => collect_accuracy(&args.fixture, &graph, args.ra_lsp_probes),
        BenchmarkLanguage::Python => empty_accuracy("rust-analyzer oracle not used for Python"),
    };
    let python_oracle = match args.language {
        BenchmarkLanguage::Java => None,
        BenchmarkLanguage::Rust => None,
        BenchmarkLanguage::Python => Some(collect_python_oracle_accuracy(&args.fixture, &graph)?),
    };
    let java_oracle = match args.language {
        BenchmarkLanguage::Java => Some(collect_java_oracle_accuracy(
            &args.fixture,
            &graph,
            &query_reports,
        )?),
        BenchmarkLanguage::Rust | BenchmarkLanguage::Python => None,
    };
    let faster_than_validation =
        validation_status.starts_with("skipped") || squeezy_total_ms < validation_ms;

    let mixed_workload = if args.language == BenchmarkLanguage::Rust {
        args.mixed_repo
            .as_ref()
            .map(|repo| run_mixed_workload(repo, args.mixed_iterations, args.ra_lsp_probes))
            .transpose()?
    } else {
        None
    };

    let stats = graph.stats();
    let report = BenchmarkReport {
        language: args.language.as_str().to_string(),
        fixture: args.fixture.display().to_string(),
        spec: args.spec.display().to_string(),
        rustc_check_ms: validation_ms,
        validation_ms,
        validation_status,
        squeezy_build_ms,
        squeezy_query_ms,
        squeezy_total_ms,
        faster_than_rustc_check: faster_than_validation,
        faster_than_validation,
        graph: GraphReport {
            files: stats.files,
            symbols: stats.symbols,
            edges: stats.edges,
            body_hits: stats.body_hits,
            references: stats.references,
            calls: stats.calls,
        },
        accuracy,
        python_oracle,
        java_oracle,
        queries: query_reports,
        mixed_workload,
    };

    write_report(&args.report, &report)?;
    print_summary(&report);
    enforce_gates(&report, args.no_speed_gate)
}

fn build_graph(root: &Path) -> Result<SemanticGraph> {
    let snapshot = WorkspaceCrawler::new(CrawlOptions::default()).crawl(root)?;
    let mut parser = RustParser::new()?;
    let (parsed, _) = parser.parse_records(&snapshot.files)?;
    Ok(SemanticGraph::from_parsed(parsed))
}

fn run_query(graph: &SemanticGraph, query: &QuerySpec) -> Result<QueryReport> {
    let actual = match query.kind.as_str() {
        "hierarchy_contains" => flatten_hierarchy(graph),
        "signature_search" => graph
            .signature_search(&SignatureQuery {
                text: query.text.clone(),
                kind: query
                    .symbol_kind
                    .as_deref()
                    .map(parse_symbol_kind)
                    .transpose()?,
                visibility: None,
                attribute: query.attribute.clone(),
            })
            .into_iter()
            .map(|symbol| format!("{:?}:{}", symbol.kind, symbol.name))
            .collect(),
        "body_search" => graph
            .body_search(&BodySearchQuery {
                text: query.text.clone(),
                owner_kind: query
                    .owner_kind
                    .as_deref()
                    .map(parse_symbol_kind)
                    .transpose()?,
                hit_kind: None::<BodyHitKind>,
            })
            .into_iter()
            .map(|hit| {
                format!(
                    "{}:{}",
                    hit.owner
                        .as_ref()
                        .map(|owner| owner.name.as_str())
                        .unwrap_or("<file>"),
                    hit.hit.text
                )
            })
            .collect(),
        "reference_search" => graph
            .reference_search(&query.text)
            .into_iter()
            .map(|hit| hit.reference.text)
            .collect(),
        "java_project_facts" => graph
            .java_project_facts()
            .iter()
            .map(|fact| format!("{}:{}:{}", fact.provider, fact.kind, fact.value))
            .collect(),
        "references_to_symbol" => {
            let to = required(&query.to, "to")?;
            let symbol = graph
                .find_symbol_by_name(to)
                .into_iter()
                .next()
                .ok_or_else(|| SqueezyError::Graph(format!("missing symbol {to}")))?;
            graph
                .references_to_symbol(&symbol.id)
                .into_iter()
                .map(|hit| hit.reference.text)
                .collect()
        }
        "call_chain" => {
            let from = required(&query.from, "from")?;
            let to = required(&query.to, "to")?;
            let from_symbols = graph.find_symbol_by_name(from);
            if from_symbols.is_empty() {
                return Err(SqueezyError::Graph(format!("missing symbol {from}")));
            }
            let to_symbols = graph.find_symbol_by_name(to);
            if to_symbols.is_empty() {
                return Err(SqueezyError::Graph(format!("missing symbol {to}")));
            }
            let mut chains = Vec::new();
            for from_symbol in &from_symbols {
                for to_symbol in &to_symbols {
                    if let Some(chain) = graph.call_chain(&from_symbol.id, &to_symbol.id, 8) {
                        chains.push(
                            chain
                                .iter()
                                .filter_map(|id| graph.symbols.get(id))
                                .map(|symbol| symbol.name.clone())
                                .collect::<Vec<_>>()
                                .join(" -> "),
                        );
                    }
                }
            }
            chains
        }
        unknown => {
            return Err(SqueezyError::Graph(format!(
                "unknown benchmark query kind {unknown}"
            )));
        }
    };

    let actual = actual.into_iter().collect::<BTreeSet<_>>();
    let expected = query
        .expected_contains
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let missing = expected.difference(&actual).cloned().collect::<Vec<_>>();
    let extras = actual.difference(&expected).cloned().collect::<Vec<_>>();

    Ok(QueryReport {
        id: query.id.clone(),
        kind: query.kind.clone(),
        expected_contains: query.expected_contains.clone(),
        actual: actual.into_iter().collect(),
        missing,
        extras,
        documented_misses: query.documented_misses.clone(),
    })
}

fn run_mixed_workload(
    repo: &Path,
    requested_scenarios: usize,
    ra_lsp_probes: usize,
) -> Result<MixedWorkloadReport> {
    let (compiler_check_ms, compiler_check_status) = time_cargo_check_optional(repo);
    let (rust_analyzer_ms, rust_analyzer_status) = time_rust_analyzer(repo);

    let build_started = Instant::now();
    let graph = build_graph(repo)?;
    let squeezy_build_ms = build_started.elapsed().as_millis();
    let accuracy = collect_accuracy(repo, &graph, ra_lsp_probes);

    let scenarios = build_mixed_scenarios(&graph);
    let scenario_indexes = select_scenarios(scenarios.len(), requested_scenarios);
    let tools = scenarios
        .iter()
        .map(|scenario| scenario.tool().to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    let query_started = Instant::now();
    let mut query_counts = BTreeMap::new();
    let mut query_time_micros: BTreeMap<String, u128> = BTreeMap::new();
    for index in scenario_indexes {
        let scenario = &scenarios[index];
        let scenario_started = Instant::now();
        run_mixed_scenario(&graph, scenario);
        let elapsed = scenario_started.elapsed().as_micros();
        increment(&mut query_counts, scenario.tool());
        *query_time_micros
            .entry(scenario.tool().to_string())
            .or_default() += elapsed;
    }
    let squeezy_query_ms = query_started.elapsed().as_millis();
    let query_time_ms = query_time_micros
        .into_iter()
        .map(|(tool, micros)| (tool, micros / 1_000))
        .collect::<BTreeMap<_, _>>();
    let squeezy_total_ms = squeezy_build_ms + squeezy_query_ms;
    let refresh_probe = run_refresh_probe(repo)?;

    Ok(MixedWorkloadReport {
        repo: repo.display().to_string(),
        requested_scenarios,
        available_scenarios: scenarios.len(),
        executed_scenarios: query_counts.values().sum(),
        tools,
        compiler_check_ms,
        compiler_check_status,
        rust_analyzer_ms,
        rust_analyzer_status,
        squeezy_build_ms,
        squeezy_query_ms,
        squeezy_total_ms,
        faster_than_compiler_check: compiler_check_ms.map(|ms| squeezy_total_ms < ms),
        faster_than_rust_analyzer: rust_analyzer_ms.map(|ms| squeezy_total_ms < ms),
        query_counts,
        query_time_ms,
        refresh_probe,
        accuracy,
    })
}

fn collect_accuracy(root: &Path, graph: &SemanticGraph, ra_lsp_probes: usize) -> AccuracyReport {
    let squeezy_symbols = collect_squeezy_symbol_scan(graph);
    let started = Instant::now();
    let (rust_analyzer_symbols, status) = collect_rust_analyzer_symbol_scan(graph);
    let rust_analyzer_symbols_ms = if status.starts_with("rust-analyzer symbols succeeded") {
        Some(started.elapsed().as_millis())
    } else {
        None
    };
    let symbols = compare_symbol_sets(&squeezy_symbols, &rust_analyzer_symbols);
    let navigation = collect_navigation_accuracy(root, graph, ra_lsp_probes);

    AccuracyReport {
        rust_analyzer_symbols_ms,
        rust_analyzer_symbol_status: status,
        symbols,
        navigation,
        limitations: vec![
            "Symbol TP/FP/FN compares declaration families both engines expose; raw rust-analyzer locals and fields are counted as excluded, not silently compared.".to_string(),
            "Navigation TP/FP/FN is sampled through rust-analyzer LSP definition/reference requests; it is a realistic loss tracker, not an exhaustive proof.".to_string(),
            "Macro-generated items, proc macros, cfg matrices, trait dispatch, deref/autoref method resolution, and external crate/stdlib references remain documented lower-confidence areas.".to_string(),
        ],
    }
}

fn empty_accuracy(status: &str) -> AccuracyReport {
    AccuracyReport {
        rust_analyzer_symbols_ms: None,
        rust_analyzer_symbol_status: status.to_string(),
        symbols: compare_symbol_sets(&SymbolScan::default(), &SymbolScan::default()),
        navigation: NavigationAccuracyReport {
            rust_analyzer_lsp_ms: None,
            rust_analyzer_lsp_status: status.to_string(),
            requested_probe_limit: 0,
            definitions: DefinitionAccuracyReport::default(),
            references: ReferenceAccuracyReport::default(),
            limitations: vec![status.to_string()],
        },
        limitations: vec![status.to_string()],
    }
}

fn collect_python_oracle_accuracy(root: &Path, graph: &SemanticGraph) -> Result<PythonOracleReport> {
    let started = Instant::now();
    let oracle = collect_python_ast_symbol_scan(root)?;
    let oracle_ms = started.elapsed().as_millis();
    let unparseable_files = oracle.unparseable_files.into_iter().collect::<BTreeSet<_>>();
    let squeezy_symbols = collect_squeezy_symbol_scan_excluding_files(graph, &unparseable_files);
    let symbols = compare_symbol_sets(&squeezy_symbols, &oracle.symbols);
    let oracle_unparseable_examples = unparseable_files.iter().take(10).cloned().collect::<Vec<_>>();
    let oracle_unparseable_files = unparseable_files.len();

    Ok(PythonOracleReport {
        oracle_ms,
        status: if oracle_unparseable_files == 0 {
            "CPython ast oracle succeeded".to_string()
        } else {
            format!(
                "CPython ast oracle succeeded with {oracle_unparseable_files} unparseable files excluded from symbol FP accounting"
            )
        },
        oracle_unparseable_files,
        oracle_unparseable_examples,
        symbols,
        limitations: vec![
            "The Python oracle uses CPython ast for declarations and does not execute imports, infer dynamic attributes, or model metaclass-generated members.".to_string(),
            "Symbol comparison is file/name/kind based so it tracks declaration loss without pretending to prove runtime dispatch.".to_string(),
            "Python files that CPython ast cannot parse are reported as oracle_unparseable and excluded from Squeezy false-positive accounting; tree-sitter recovery remains useful for production editing workflows.".to_string(),
        ],
    })
}

fn collect_squeezy_symbol_scan(graph: &SemanticGraph) -> SymbolScan {
    collect_squeezy_symbol_scan_excluding_files(graph, &BTreeSet::new())
}

fn collect_squeezy_symbol_scan_excluding_files(
    graph: &SemanticGraph,
    excluded_files: &BTreeSet<String>,
) -> SymbolScan {
    let mut scan = SymbolScan::default();
    for symbol in graph.symbols.values() {
        scan.raw_total += 1;
        match normalize_squeezy_kind(symbol.kind) {
            Some(kind) => {
                let Some(file) = graph.files.get(&symbol.file_id) else {
                    increment(&mut scan.excluded_by_kind, "MissingFile");
                    continue;
                };
                if excluded_files.contains(&file.relative_path) {
                    increment(&mut scan.excluded_by_kind, "OracleUnparseableFile");
                    continue;
                }
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

#[derive(Debug, Deserialize)]
struct PythonAstOracleOutput {
    rows: Vec<[String; 3]>,
    unparseable_files: Vec<String>,
}

#[derive(Debug)]
struct PythonAstSymbolScan {
    symbols: SymbolScan,
    unparseable_files: Vec<String>,
}

fn collect_python_ast_symbol_scan(root: &Path) -> Result<PythonAstSymbolScan> {
    let output = Command::new("python3")
        .arg("-c")
        .arg(PYTHON_AST_ORACLE)
        .arg(root)
        .output()
        .map_err(|err| SqueezyError::Graph(format!("failed to run Python AST oracle: {err}")))?;
    if !output.status.success() {
        return Err(SqueezyError::Graph(format!(
            "Python AST oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let output: PythonAstOracleOutput = serde_json::from_slice(&output.stdout)
        .map_err(|err| SqueezyError::Graph(format!("invalid Python AST oracle JSON: {err}")))?;
    let mut scan = SymbolScan::default();
    for [file, kind, name] in output.rows {
        scan.raw_total += 1;
        increment_symbol(
            &mut scan.counts,
            SymbolKey {
                file,
                kind,
                name: normalize_symbol_name(&name),
            },
        );
    }
    Ok(PythonAstSymbolScan {
        symbols: scan,
        unparseable_files: output.unparseable_files,
    })
}

const PYTHON_AST_ORACLE: &str = r#"
import ast
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1]).resolve()
rows = []
unparseable_files = []

class Visitor(ast.NodeVisitor):
    def __init__(self, rel):
        self.rel = rel
        self.parents = []

    def visit_ClassDef(self, node):
        rows.append([self.rel, "Class", node.name])
        self.parents.append("Class")
        self.generic_visit(node)
        self.parents.pop()

    def visit_FunctionDef(self, node):
        kind = "Method" if self.parents and self.parents[-1] == "Class" else "Function"
        rows.append([self.rel, kind, node.name])
        self.parents.append(kind)
        self.generic_visit(node)
        self.parents.pop()

    visit_AsyncFunctionDef = visit_FunctionDef

for path in sorted(root.rglob("*.py")):
    rel = path.relative_to(root).as_posix()
    try:
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    except (SyntaxError, UnicodeDecodeError):
        unparseable_files.append(rel)
        continue
    Visitor(rel).visit(tree)

print(json.dumps({"rows": rows, "unparseable_files": unparseable_files}))
"#;

fn time_java_oracle_optional(root: &Path) -> (u128, String) {
    if !command_exists("java") {
        return (0, "skipped: java not found".to_string());
    }
    let started = Instant::now();
    match collect_java_compiler_tree_symbol_scan(root) {
        Ok((_, status)) if status.starts_with("JDK compiler tree oracle succeeded") => {
            (started.elapsed().as_millis(), status)
        }
        Ok((_, status)) => (0, format!("skipped: {status}")),
        Err(err) => (0, format!("skipped: Java oracle failed: {err}")),
    }
}

fn collect_java_oracle_accuracy(
    root: &Path,
    graph: &SemanticGraph,
    queries: &[QueryReport],
) -> Result<JavaOracleReport> {
    if !command_exists("java") {
        return Ok(JavaOracleReport {
            oracle_ms: None,
            status: "skipped: java not found".to_string(),
            symbols: compare_symbol_sets(&collect_squeezy_symbol_scan(graph), &SymbolScan::default()),
            navigation: collect_query_oracle_accuracy(queries),
            limitations: java_oracle_limitations(),
        });
    }
    let started = Instant::now();
    match collect_java_compiler_tree_symbol_scan(root) {
        Ok((oracle, status)) if status.starts_with("JDK compiler tree oracle succeeded") => {
            let oracle_ms = started.elapsed().as_millis();
            let squeezy_symbols = collect_squeezy_symbol_scan(graph);
            Ok(JavaOracleReport {
                oracle_ms: Some(oracle_ms),
                status,
                symbols: compare_symbol_sets(&squeezy_symbols, &oracle),
                navigation: collect_query_oracle_accuracy(queries),
                limitations: java_oracle_limitations(),
            })
        }
        Ok((_, status)) => Ok(JavaOracleReport {
            oracle_ms: None,
            status: format!("skipped: {status}"),
            symbols: compare_symbol_sets(&collect_squeezy_symbol_scan(graph), &SymbolScan::default()),
            navigation: collect_query_oracle_accuracy(queries),
            limitations: java_oracle_limitations(),
        }),
        Err(err) => Ok(JavaOracleReport {
            oracle_ms: None,
            status: format!("skipped: Java oracle failed: {err}"),
            symbols: compare_symbol_sets(&collect_squeezy_symbol_scan(graph), &SymbolScan::default()),
            navigation: collect_query_oracle_accuracy(queries),
            limitations: java_oracle_limitations(),
        }),
    }
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
    let false_negative = queries.iter().map(|query| query.missing.len()).sum::<usize>();
    // Query specs use expected_contains, not an exhaustive expected set, so
    // extra results stay visible on each query but are not counted as oracle FP.
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

fn java_oracle_limitations() -> Vec<String> {
    vec![
        "The Java oracle uses the JDK compiler tree API for declarations only and does not require successful type attribution.".to_string(),
        "Symbol comparison is file/name/kind based; overload resolution, dispatch, generated sources, annotation processors, and external libraries remain separate navigation-loss areas.".to_string(),
        "If java or a JDK compiler is unavailable, the oracle is skipped while fixture query gates still run.".to_string(),
    ]
}

#[derive(Debug, Deserialize)]
struct JavaOracleOutput {
    rows: Vec<[String; 3]>,
}

fn collect_java_compiler_tree_symbol_scan(root: &Path) -> Result<(SymbolScan, String)> {
    let temp = temp_dir("squeezy-java-oracle")?;
    let oracle_path = temp.join("JavaOracle.java");
    fs::write(&oracle_path, JAVA_COMPILER_TREE_ORACLE)?;
    let output = Command::new("java")
        .arg(&oracle_path)
        .arg(root)
        .output()
        .map_err(|err| SqueezyError::Graph(format!("failed to run Java oracle: {err}")))?;
    if !output.status.success() {
        return Ok((
            SymbolScan::default(),
            format!(
                "Java oracle unavailable: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    let output: JavaOracleOutput = serde_json::from_slice(&output.stdout)
        .map_err(|err| SqueezyError::Graph(format!("invalid Java oracle JSON: {err}")))?;
    let mut scan = SymbolScan::default();
    for [file, kind, name] in output.rows {
        scan.raw_total += 1;
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
            "JDK compiler tree oracle succeeded with {} declaration symbols",
            symbol_count(&scan.counts)
        ),
    ))
}

const JAVA_COMPILER_TREE_ORACLE: &str = r#"
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import javax.tools.JavaCompiler;
import javax.tools.StandardJavaFileManager;
import javax.tools.ToolProvider;
import com.sun.source.tree.ClassTree;
import com.sun.source.tree.CompilationUnitTree;
import com.sun.source.tree.MethodTree;
import com.sun.source.tree.Tree;
import com.sun.source.util.JavacTask;
import com.sun.source.util.TreeScanner;

public class JavaOracle {
  record Row(String file, String kind, String name) {}

  public static void main(String[] args) throws Exception {
    JavaCompiler compiler = ToolProvider.getSystemJavaCompiler();
    if (compiler == null) {
      System.err.println("JDK compiler is not available");
      System.exit(2);
    }
    Path root = Path.of(args[0]).toAbsolutePath().normalize();
    List<Path> files = Files.walk(root)
      .filter(path -> path.toString().endsWith(".java"))
      .sorted()
      .toList();
    List<Row> rows = new ArrayList<>();
    try (StandardJavaFileManager manager = compiler.getStandardFileManager(null, null, StandardCharsets.UTF_8)) {
      Iterable units = manager.getJavaFileObjectsFromPaths(files);
      JavacTask task = (JavacTask) compiler.getTask(null, manager, null, List.of("-proc:none"), null, units);
      for (CompilationUnitTree unit : task.parse()) {
        String rel = root.relativize(Path.of(unit.getSourceFile().toUri()).toAbsolutePath().normalize()).toString().replace('\\', '/');
        new Scanner(rel, rows).scan(unit, null);
      }
    }
    rows.sort(Comparator.comparing(Row::file).thenComparing(Row::kind).thenComparing(Row::name));
    StringBuilder out = new StringBuilder();
    out.append("{\"rows\":[");
    for (int i = 0; i < rows.size(); i++) {
      Row row = rows.get(i);
      if (i > 0) out.append(',');
      out.append("[\"").append(escape(row.file())).append("\",\"")
        .append(escape(row.kind())).append("\",\"")
        .append(escape(row.name())).append("\"]");
    }
    out.append("]}");
    System.out.println(out);
  }

  static class Scanner extends TreeScanner<Void, Void> {
    private final String file;
    private final List<Row> rows;
    private final ArrayDeque<String> classes = new ArrayDeque<>();

    Scanner(String file, List<Row> rows) {
      this.file = file;
      this.rows = rows;
    }

    @Override
    public Void visitClass(ClassTree node, Void unused) {
      String kind = switch (node.getKind()) {
        case CLASS -> "Class";
        case INTERFACE, ANNOTATION_TYPE -> "Trait";
        case ENUM -> "Enum";
        case RECORD -> "Struct";
        default -> "Class";
      };
      String name = node.getSimpleName().toString();
      if (name.isEmpty()) {
        return super.visitClass(node, unused);
      }
      rows.add(new Row(file, kind, name));
      classes.push(name);
      super.visitClass(node, unused);
      classes.pop();
      return null;
    }

    @Override
    public Void visitMethod(MethodTree node, Void unused) {
      String name = node.getName().toString();
      if ("<init>".equals(name) && !classes.isEmpty()) {
        name = classes.peek();
      }
      rows.add(new Row(file, "Method", name));
      return super.visitMethod(node, unused);
    }
  }

  static String escape(String value) {
    return value.replace("\\", "\\\\").replace("\"", "\\\"");
  }
}
"#;

fn collect_rust_analyzer_symbol_scan(graph: &SemanticGraph) -> (SymbolScan, String) {
    let Some(program) = rust_analyzer_program() else {
        return (SymbolScan::default(), "rust-analyzer not found".to_string());
    };

    let mut records = graph
        .files
        .values()
        .filter(|record| record.language == LanguageKind::Rust)
        .collect::<Vec<_>>();
    records.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

    let mut scan = SymbolScan::default();
    let mut failures = Vec::new();
    for record in &records {
        match rust_analyzer_symbols_for_file(&program, record) {
            Ok(Some(file_scan)) => {
                merge_symbol_scan(&mut scan, file_scan);
            }
            Ok(None) => {
                scan.skipped_non_utf8_files += 1;
            }
            Err(err) => {
                failures.push(format!("{}: {err}", record.relative_path));
            }
        }
    }

    if failures.is_empty() {
        (
            scan.clone(),
            format!(
                "rust-analyzer symbols succeeded for {} Rust files; skipped {} non-UTF-8 Rust files",
                records.len() - scan.skipped_non_utf8_files,
                scan.skipped_non_utf8_files
            ),
        )
    } else {
        (
            scan,
            format!(
                "rust-analyzer symbols partially failed for {}/{} Rust files: {}",
                failures.len(),
                records.len(),
                failures
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        )
    }
}

fn rust_analyzer_symbols_for_file(
    program: &str,
    record: &squeezy_workspace::FileRecord,
) -> Result<Option<SymbolScan>> {
    let source = match fs::read_to_string(&record.path) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::InvalidData => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut child = Command::new(program)
        .arg("symbols")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| SqueezyError::Graph("failed to open rust-analyzer stdin".to_string()))?;
    stdin.write_all(source.as_bytes())?;
    drop(stdin);

    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SqueezyError::Graph(format!(
            "rust-analyzer symbols failed with {}: {}",
            output.status,
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut scan = SymbolScan::default();
    for line in stdout.lines() {
        let Some((raw_kind, key)) = parse_rust_analyzer_symbol_line(line, &record.relative_path)
        else {
            continue;
        };
        scan.raw_total += 1;
        if let Some(key) = key {
            increment_symbol(&mut scan.counts, key);
        } else {
            increment(&mut scan.excluded_by_kind, &raw_kind);
        }
    }
    Ok(Some(scan))
}

fn parse_rust_analyzer_symbol_line(line: &str, file: &str) -> Option<(String, Option<SymbolKey>)> {
    let label = extract_quoted_field(line, "label")?;
    let raw_kind = extract_symbol_kind(line)?;
    let key = normalize_rust_analyzer_kind(&raw_kind).map(|kind| SymbolKey {
        file: file.to_string(),
        kind,
        name: normalize_symbol_name(&label),
    });
    Some((raw_kind, key))
}

fn extract_quoted_field(line: &str, field: &str) -> Option<String> {
    let prefix = format!("{field}: \"");
    let start = line.find(&prefix)? + prefix.len();
    let mut escaped = false;
    let mut value = String::new();
    for ch in line[start..].chars() {
        if escaped {
            value.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(value);
        } else {
            value.push(ch);
        }
    }
    None
}

fn extract_symbol_kind(line: &str) -> Option<String> {
    let prefix = "kind: SymbolKind(";
    let start = line.find(prefix)? + prefix.len();
    let rest = &line[start..];
    let end = rest.find(')')?;
    Some(rest[..end].to_string())
}

fn normalize_rust_analyzer_kind(kind: &str) -> Option<String> {
    match kind {
        "Module" => Some("Module".to_string()),
        "Struct" => Some("Struct".to_string()),
        "Enum" => Some("Enum".to_string()),
        "Union" => Some("Union".to_string()),
        "Trait" => Some("Trait".to_string()),
        "Impl" => Some("Impl".to_string()),
        "Function" => Some("Function".to_string()),
        "Method" => Some("Method".to_string()),
        "Const" => Some("Const".to_string()),
        "Static" => Some("Static".to_string()),
        "TypeAlias" => Some("TypeAlias".to_string()),
        "Macro" => Some("Macro".to_string()),
        _ => None,
    }
}

fn normalize_squeezy_kind(kind: SymbolKind) -> Option<String> {
    match kind {
        SymbolKind::Class => Some("Class".to_string()),
        SymbolKind::Module => Some("Module".to_string()),
        SymbolKind::Struct => Some("Struct".to_string()),
        SymbolKind::Enum => Some("Enum".to_string()),
        SymbolKind::Union => Some("Union".to_string()),
        SymbolKind::Trait => Some("Trait".to_string()),
        SymbolKind::Impl => Some("Impl".to_string()),
        SymbolKind::Function | SymbolKind::Test => Some("Function".to_string()),
        SymbolKind::Method => Some("Method".to_string()),
        SymbolKind::Const => Some("Const".to_string()),
        SymbolKind::Static => Some("Static".to_string()),
        SymbolKind::TypeAlias => Some("TypeAlias".to_string()),
        SymbolKind::Macro => Some("Macro".to_string()),
        SymbolKind::Crate
        | SymbolKind::File
        | SymbolKind::Field
        | SymbolKind::Variant
        | SymbolKind::Unknown => None,
    }
}

fn normalize_symbol_name(name: &str) -> String {
    trim_impl_header(&name.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn trim_impl_header(raw: &str) -> String {
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix("unsafe ").unwrap_or(trimmed);
    let Some(rest) = trimmed.strip_prefix("impl") else {
        return trimmed.to_string();
    };
    let Some(next) = rest.chars().next() else {
        return trimmed.to_string();
    };
    if !next.is_whitespace() && next != '<' {
        return trimmed.to_string();
    }

    let mut rest = rest.trim_start();
    if rest.starts_with('<') {
        let mut depth = 0usize;
        let mut close_index = None;
        let mut previous = None;
        for (index, ch) in rest.char_indices() {
            match ch {
                '<' => depth += 1,
                '>' if previous != Some('-') => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        close_index = Some(index + ch.len_utf8());
                        break;
                    }
                }
                _ => {}
            }
            previous = Some(ch);
        }
        if let Some(index) = close_index {
            rest = rest[index..].trim_start();
        }
    }
    rest.split_once(" where ")
        .map(|(before, _)| before)
        .unwrap_or(rest)
        .trim_end_matches(',')
        .to_string()
}

fn compare_symbol_sets(squeezy: &SymbolScan, rust_analyzer: &SymbolScan) -> AccuracySetReport {
    let true_positive = squeezy
        .counts
        .iter()
        .map(|(key, count)| count.min(rust_analyzer.counts.get(key).unwrap_or(&0)))
        .sum::<usize>();
    let false_positive = count_difference(&squeezy.counts, &rust_analyzer.counts);
    let false_negative = count_difference(&rust_analyzer.counts, &squeezy.counts);
    let precision = ratio(true_positive, true_positive + false_positive);
    let recall = ratio(true_positive, true_positive + false_negative);

    AccuracySetReport {
        compared_kinds: vec![
            "Class".to_string(),
            "Module".to_string(),
            "Struct".to_string(),
            "Enum".to_string(),
            "Union".to_string(),
            "Trait".to_string(),
            "Impl".to_string(),
            "Function".to_string(),
            "Method".to_string(),
            "Const".to_string(),
            "Static".to_string(),
            "TypeAlias".to_string(),
            "Macro".to_string(),
        ],
        rust_analyzer_raw_total: rust_analyzer.raw_total,
        rust_analyzer_total: symbol_count(&rust_analyzer.counts),
        rust_analyzer_unique: rust_analyzer.counts.len(),
        rust_analyzer_excluded_by_kind: rust_analyzer.excluded_by_kind.clone(),
        rust_analyzer_skipped_non_utf8_files: rust_analyzer.skipped_non_utf8_files,
        squeezy_raw_total: squeezy.raw_total,
        squeezy_total: symbol_count(&squeezy.counts),
        squeezy_unique: squeezy.counts.len(),
        squeezy_excluded_by_kind: squeezy.excluded_by_kind.clone(),
        true_positive,
        false_positive,
        false_negative,
        precision,
        recall,
        false_positive_examples: difference_examples(&squeezy.counts, &rust_analyzer.counts),
        false_negative_examples: difference_examples(&rust_analyzer.counts, &squeezy.counts),
    }
}

fn collect_navigation_accuracy(
    root: &Path,
    graph: &SemanticGraph,
    probe_limit: usize,
) -> NavigationAccuracyReport {
    if probe_limit == 0 {
        return NavigationAccuracyReport {
            rust_analyzer_lsp_ms: None,
            rust_analyzer_lsp_status: "disabled by --ra-lsp-probes 0".to_string(),
            requested_probe_limit: probe_limit,
            definitions: DefinitionAccuracyReport::default(),
            references: ReferenceAccuracyReport::default(),
            limitations: navigation_limitations(),
        };
    }

    let started = Instant::now();
    let mut client = match RustAnalyzerLsp::start(root) {
        Ok(client) => client,
        Err(err) => {
            return NavigationAccuracyReport {
                rust_analyzer_lsp_ms: None,
                rust_analyzer_lsp_status: format!("rust-analyzer LSP unavailable: {err}"),
                requested_probe_limit: probe_limit,
                definitions: DefinitionAccuracyReport::default(),
                references: ReferenceAccuracyReport::default(),
                limitations: navigation_limitations(),
            };
        }
    };

    let definitions = compare_definition_probes(root, graph, &mut client, probe_limit);
    let references = compare_reference_probes(root, graph, &mut client, probe_limit);
    let elapsed = started.elapsed().as_millis();
    let status = match (&definitions, &references) {
        (Ok(_), Ok(_)) => "rust-analyzer LSP definition/reference probes succeeded".to_string(),
        (Err(err), _) => format!("rust-analyzer LSP definition probes failed: {err}"),
        (_, Err(err)) => format!("rust-analyzer LSP reference probes failed: {err}"),
    };

    NavigationAccuracyReport {
        rust_analyzer_lsp_ms: (definitions.is_ok() && references.is_ok()).then_some(elapsed),
        rust_analyzer_lsp_status: status,
        requested_probe_limit: probe_limit,
        definitions: definitions.unwrap_or_default(),
        references: references.unwrap_or_default(),
        limitations: navigation_limitations(),
    }
}

fn navigation_limitations() -> Vec<String> {
    vec![
        "Definition probes compare Squeezy resolved call and macro edge targets with rust-analyzer LSP definitions for sampled call sites.".to_string(),
        "Reference probes compare Squeezy references_to_symbol results with rust-analyzer LSP references for sampled declarations, excluding declarations because the selected symbol already supplies the definition span.".to_string(),
        "Samples are deterministic and capped; increase --ra-lsp-probes for deeper local audits.".to_string(),
        "External dependency definitions are counted as rust-analyzer-only misses because Squeezy currently indexes workspace files only.".to_string(),
    ]
}

fn compare_definition_probes(
    root: &Path,
    graph: &SemanticGraph,
    client: &mut RustAnalyzerLsp,
    probe_limit: usize,
) -> Result<DefinitionAccuracyReport> {
    let (available_probes, probes) = build_definition_probes(graph, probe_limit)?;
    let mut report = DefinitionAccuracyReport {
        available_probes,
        probes: probes.len(),
        ..DefinitionAccuracyReport::default()
    };

    for probe in probes {
        client.did_open(&probe.uri, &probe.path)?;
        let ra_locations = client.definition(&probe.uri, probe.position)?;
        let squeezy_has_target = probe.squeezy_target.is_some();
        let squeezy_matches = probe
            .squeezy_target
            .as_ref()
            .and_then(|id| graph.symbols.get(id))
            .map(|symbol| {
                ra_locations
                    .iter()
                    .any(|location| location_matches_symbol(root, graph, location, symbol))
            })
            .unwrap_or(false);

        match (ra_locations.is_empty(), squeezy_has_target, squeezy_matches) {
            (false, true, true) => report.true_positive += 1,
            (false, false, _) => {
                report.false_negative += 1;
                push_example(
                    &mut report.examples,
                    format!(
                        "FN definition {}: RA -> {}, Squeezy unresolved",
                        probe.label,
                        render_locations(&ra_locations)
                    ),
                );
            }
            (false, true, false) => {
                report.false_positive += 1;
                report.false_negative += 1;
                report.wrong_target += 1;
                push_example(
                    &mut report.examples,
                    format!(
                        "Wrong definition {}: RA -> {}, Squeezy -> {}",
                        probe.label,
                        render_locations(&ra_locations),
                        probe
                            .squeezy_target
                            .as_ref()
                            .map(|id| id.0.as_str())
                            .unwrap_or("<none>")
                    ),
                );
            }
            (true, true, false) => {
                report.false_positive += 1;
                report.squeezy_only += 1;
                push_example(
                    &mut report.examples,
                    format!(
                        "Squeezy-only definition {}: RA unresolved, Squeezy -> {}",
                        probe.label,
                        probe
                            .squeezy_target
                            .as_ref()
                            .map(|id| id.0.as_str())
                            .unwrap_or("<none>")
                    ),
                );
            }
            (true, false, _) => report.unresolved_agreement += 1,
            (true, true, true) => unreachable!("matched target requires an RA location"),
        }
    }

    report.precision = ratio(
        report.true_positive,
        report.true_positive + report.false_positive,
    );
    report.recall = ratio(
        report.true_positive,
        report.true_positive + report.false_negative,
    );
    Ok(report)
}

fn compare_reference_probes(
    root: &Path,
    graph: &SemanticGraph,
    client: &mut RustAnalyzerLsp,
    probe_limit: usize,
) -> Result<ReferenceAccuracyReport> {
    let (available_symbols, probes) = build_reference_probes(root, graph, probe_limit)?;
    let mut report = ReferenceAccuracyReport {
        available_symbols,
        symbols_sampled: probes.len(),
        ..ReferenceAccuracyReport::default()
    };

    for probe in probes {
        client.did_open(&probe.uri, &probe.path)?;
        let ra = client
            .references(&probe.uri, probe.position)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let squeezy = graph
            .references_to_symbol(&probe.symbol_id)
            .into_iter()
            .filter_map(|hit| location_key_for_reference_hit(graph, &hit, &probe.name))
            .collect::<BTreeSet<_>>();

        let tp = squeezy.intersection(&ra).count();
        let fp = squeezy.difference(&ra).cloned().collect::<Vec<_>>();
        let fn_ = ra.difference(&squeezy).cloned().collect::<Vec<_>>();
        report.true_positive += tp;
        report.false_positive += fp.len();
        report.false_negative += fn_.len();

        if !fp.is_empty() {
            push_example(
                &mut report.false_positive_examples,
                format!(
                    "{} FP refs: {}",
                    probe.label,
                    fp.iter()
                        .take(5)
                        .map(LocationKey::render)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
        if !fn_.is_empty() {
            push_example(
                &mut report.false_negative_examples,
                format!(
                    "{} FN refs: {}",
                    probe.label,
                    fn_.iter()
                        .take(5)
                        .map(LocationKey::render)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
    }

    report.precision = ratio(
        report.true_positive,
        report.true_positive + report.false_positive,
    );
    report.recall = ratio(
        report.true_positive,
        report.true_positive + report.false_negative,
    );
    Ok(report)
}

fn build_definition_probes(
    graph: &SemanticGraph,
    limit: usize,
) -> Result<(usize, Vec<DefinitionProbe>)> {
    let mut probes = Vec::new();
    let mut edges = graph
        .edges()
        .iter()
        .filter(|edge| matches!(edge.kind, EdgeKind::Calls | EdgeKind::InvokesMacro))
        .filter_map(|edge| {
            let span = edge.span?;
            let from = graph.symbols.get(&edge.from)?;
            let file = graph.files.get(&from.file_id)?;
            Some((file.relative_path.clone(), span.start_byte, edge))
        })
        .collect::<Vec<_>>();
    edges.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then(left.1.cmp(&right.1))
            .then(left.2.target_text.cmp(&right.2.target_text))
    });
    let available = edges.len();
    let selected = select_scenarios(available, limit);

    for index in selected {
        let (_, _, edge) = edges[index];
        let Some(span) = edge.span else {
            continue;
        };
        let Some(from) = graph.symbols.get(&edge.from) else {
            continue;
        };
        let Some(file) = graph.files.get(&from.file_id) else {
            continue;
        };
        let source = fs::read_to_string(&file.path)?;
        let byte = probe_byte_for_edge(
            &source,
            span.start_byte as usize,
            span.end_byte as usize,
            &edge.target_text,
        );
        let position = byte_to_lsp_position(&source, byte);
        probes.push(DefinitionProbe {
            label: format!(
                "{}:{}:{} {}",
                file.relative_path,
                position.line + 1,
                position.character + 1,
                edge.target_text
            ),
            uri: path_to_file_uri(&file.path)?,
            path: file.path.clone(),
            position,
            squeezy_target: edge.to.clone(),
        });
    }

    Ok((available, probes))
}

fn build_reference_probes(
    _root: &Path,
    graph: &SemanticGraph,
    limit: usize,
) -> Result<(usize, Vec<ReferenceProbe>)> {
    let mut symbols = graph
        .symbols
        .values()
        .filter(|symbol| {
            matches!(
                symbol.kind,
                SymbolKind::Struct
                    | SymbolKind::Enum
                    | SymbolKind::Union
                    | SymbolKind::Trait
                    | SymbolKind::Function
                    | SymbolKind::Method
                    | SymbolKind::TypeAlias
                    | SymbolKind::Const
                    | SymbolKind::Static
                    | SymbolKind::Macro
            ) && symbol.name.len() >= 3
        })
        .collect::<Vec<_>>();
    symbols.sort_by(|left, right| {
        left.file_id
            .0
            .cmp(&right.file_id.0)
            .then(left.span.start_byte.cmp(&right.span.start_byte))
            .then(left.name.cmp(&right.name))
    });
    let available = symbols.len();
    let selected = select_scenarios(available, limit);

    let mut probes = Vec::new();
    for index in selected {
        let symbol = symbols[index];
        let Some(file) = graph.files.get(&symbol.file_id) else {
            continue;
        };
        let source = fs::read_to_string(&file.path)?;
        let byte = probe_byte_for_symbol(
            &source,
            symbol.span.start_byte as usize,
            symbol.span.end_byte as usize,
            &symbol.name,
        );
        let position = byte_to_lsp_position(&source, byte);
        probes.push(ReferenceProbe {
            label: format!(
                "{}:{}:{} {}",
                file.relative_path,
                position.line + 1,
                position.character + 1,
                symbol.name
            ),
            uri: path_to_file_uri(&file.path)?,
            path: file.path.clone(),
            position,
            symbol_id: symbol.id.clone(),
            name: symbol.name.clone(),
        });
    }

    Ok((available, probes))
}

fn location_key_for_reference_hit(
    graph: &SemanticGraph,
    hit: &squeezy_graph::ReferenceHit,
    name: &str,
) -> Option<LocationKey> {
    let file = graph.files.get(&hit.reference.file_id)?;
    let source = fs::read_to_string(&file.path).ok()?;
    let start = hit.reference.span.start_byte as usize;
    let end = (hit.reference.span.end_byte as usize).min(source.len());
    let slice = source.get(start.min(end)..end).unwrap_or_default();
    let byte = slice
        .find(name)
        .map(|index| start + index)
        .unwrap_or(hit.reference.span.start_byte as usize);
    let position = byte_to_lsp_position(&source, byte);
    Some(LocationKey {
        file: file.relative_path.clone(),
        line: position.line,
        character: position.character,
    })
}

fn probe_byte_for_edge(source: &str, start: usize, end: usize, target_text: &str) -> usize {
    let end = end.min(source.len());
    let start = start.min(end);
    let slice = source.get(start..end).unwrap_or_default();
    let needle = target_identifier(target_text);
    slice
        .rfind(&needle)
        .map(|index| start + index)
        .unwrap_or(start)
}

fn probe_byte_for_symbol(source: &str, start: usize, end: usize, name: &str) -> usize {
    let end = end.min(source.len());
    let start = start.min(end);
    let slice = source.get(start..end).unwrap_or_default();
    let needle = target_identifier(name);
    slice
        .find(&needle)
        .map(|index| start + index)
        .unwrap_or(start)
}

fn target_identifier(text: &str) -> String {
    let before_bang = text.split('!').next().unwrap_or(text);
    let before_call = before_bang.split('(').next().unwrap_or(before_bang);
    before_call
        .rsplit(|ch| ['.', ':', '<', '>', '&', ' ', '\t', '\n'].contains(&ch))
        .find(|part| !part.is_empty())
        .unwrap_or(before_call)
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .to_string()
}

fn location_matches_symbol(
    root: &Path,
    graph: &SemanticGraph,
    location: &LocationKey,
    symbol: &squeezy_graph::GraphSymbol,
) -> bool {
    let Some(file) = graph.files.get(&symbol.file_id) else {
        return false;
    };
    if location.file != file.relative_path {
        return false;
    }
    let Ok(source) = fs::read_to_string(root.join(&file.relative_path)) else {
        return false;
    };
    line_char_to_byte(&source, location.line, location.character)
        .map(|byte| symbol.span.contains_byte(byte as u32))
        .unwrap_or(false)
}

fn render_locations(locations: &[LocationKey]) -> String {
    locations
        .iter()
        .take(5)
        .map(LocationKey::render)
        .collect::<Vec<_>>()
        .join(", ")
}

fn push_example(examples: &mut Vec<String>, example: String) {
    if examples.len() < 20 {
        examples.push(example);
    }
}

fn merge_symbol_scan(target: &mut SymbolScan, source: SymbolScan) {
    target.raw_total += source.raw_total;
    target.skipped_non_utf8_files += source.skipped_non_utf8_files;
    for (key, count) in source.counts {
        *target.counts.entry(key).or_default() += count;
    }
    for (kind, count) in source.excluded_by_kind {
        *target.excluded_by_kind.entry(kind).or_default() += count;
    }
}

fn increment_symbol(counts: &mut BTreeMap<SymbolKey, usize>, key: SymbolKey) {
    *counts.entry(key).or_default() += 1;
}

fn symbol_count(counts: &BTreeMap<SymbolKey, usize>) -> usize {
    counts.values().sum()
}

fn count_difference(
    left: &BTreeMap<SymbolKey, usize>,
    right: &BTreeMap<SymbolKey, usize>,
) -> usize {
    left.iter()
        .map(|(key, count)| count.saturating_sub(*right.get(key).unwrap_or(&0)))
        .sum()
}

fn difference_examples(
    left: &BTreeMap<SymbolKey, usize>,
    right: &BTreeMap<SymbolKey, usize>,
) -> Vec<String> {
    left.iter()
        .filter_map(|(key, count)| {
            let extra = count.saturating_sub(*right.get(key).unwrap_or(&0));
            match extra {
                0 => None,
                1 => Some(key.render()),
                _ => Some(format!("{} x{}", key.render(), extra)),
            }
        })
        .take(20)
        .collect()
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        1.0
    } else {
        ((numerator as f64 / denominator as f64) * 10_000.0).round() / 10_000.0
    }
}

#[derive(Debug, Clone)]
enum MixedScenario {
    HierarchyAll {
        depth: usize,
    },
    HierarchyRoot {
        root: SymbolId,
        depth: usize,
    },
    SymbolLookup {
        name: String,
    },
    SignatureSearch {
        text: String,
        kind: Option<SymbolKind>,
    },
    BodySearch {
        text: String,
        hit_kind: Option<BodyHitKind>,
    },
    ReferenceSearch {
        text: String,
    },
    ReferencesToSymbol {
        symbol: SymbolId,
    },
    Callees {
        symbol: SymbolId,
    },
    Callers {
        symbol: SymbolId,
    },
    CallChain {
        from: SymbolId,
        to: SymbolId,
    },
}

impl MixedScenario {
    fn tool(&self) -> &'static str {
        match self {
            MixedScenario::HierarchyAll { .. } | MixedScenario::HierarchyRoot { .. } => "hierarchy",
            MixedScenario::SymbolLookup { .. } => "symbol_lookup",
            MixedScenario::SignatureSearch { .. } => "signature_search",
            MixedScenario::BodySearch { .. } => "body_search",
            MixedScenario::ReferenceSearch { .. } => "reference_search",
            MixedScenario::ReferencesToSymbol { .. } => "references_to_symbol",
            MixedScenario::Callees { .. } => "callees",
            MixedScenario::Callers { .. } => "callers",
            MixedScenario::CallChain { .. } => "call_chain",
        }
    }
}

fn build_mixed_scenarios(graph: &SemanticGraph) -> Vec<MixedScenario> {
    let mut symbols = graph
        .symbols
        .values()
        .filter(|symbol| !symbol.name.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    symbols.sort_by(|left, right| {
        format!("{:?}", left.kind)
            .cmp(&format!("{:?}", right.kind))
            .then(left.name.cmp(&right.name))
            .then(left.file_id.0.cmp(&right.file_id.0))
            .then(left.span.start_byte.cmp(&right.span.start_byte))
    });

    let names = symbols
        .iter()
        .filter(|symbol| symbol.kind != SymbolKind::File)
        .map(|symbol| symbol.name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    let mut scenarios = Vec::new();
    for depth in [1, 2, 4, 8, 16] {
        scenarios.push(MixedScenario::HierarchyAll { depth });
    }

    for symbol in &symbols {
        if symbol.kind == SymbolKind::File {
            scenarios.push(MixedScenario::HierarchyRoot {
                root: symbol.id.clone(),
                depth: 4,
            });
            continue;
        }

        scenarios.push(MixedScenario::SymbolLookup {
            name: symbol.name.clone(),
        });
        scenarios.push(MixedScenario::SignatureSearch {
            text: symbol.name.clone(),
            kind: None,
        });
        scenarios.push(MixedScenario::SignatureSearch {
            text: symbol.name.clone(),
            kind: Some(symbol.kind),
        });
        scenarios.push(MixedScenario::Callees {
            symbol: symbol.id.clone(),
        });
        scenarios.push(MixedScenario::Callers {
            symbol: symbol.id.clone(),
        });
        scenarios.push(MixedScenario::ReferencesToSymbol {
            symbol: symbol.id.clone(),
        });
    }

    for name in &names {
        scenarios.push(MixedScenario::ReferenceSearch { text: name.clone() });
        scenarios.push(MixedScenario::BodySearch {
            text: name.clone(),
            hit_kind: None,
        });
        for hit_kind in [
            BodyHitKind::Identifier,
            BodyHitKind::Type,
            BodyHitKind::Path,
            BodyHitKind::Call,
            BodyHitKind::Macro,
        ] {
            scenarios.push(MixedScenario::BodySearch {
                text: name.clone(),
                hit_kind: Some(hit_kind),
            });
        }
    }

    for edge in graph.edges() {
        if matches!(edge.kind, EdgeKind::Calls | EdgeKind::InvokesMacro)
            && let Some(to) = &edge.to
        {
            scenarios.push(MixedScenario::CallChain {
                from: edge.from.clone(),
                to: to.clone(),
            });
        }
    }

    scenarios
}

fn select_scenarios(available: usize, requested: usize) -> Vec<usize> {
    if requested == 0 || requested >= available {
        return (0..available).collect();
    }

    let mut rng = DeterministicRng::new(0x5eed_5eed_51ee_ee55_u64);
    let mut selected = BTreeSet::new();
    while selected.len() < requested {
        selected.insert(rng.next_usize(available));
    }
    selected.into_iter().collect()
}

fn run_mixed_scenario(graph: &SemanticGraph, scenario: &MixedScenario) -> usize {
    match scenario {
        MixedScenario::HierarchyAll { depth } => graph.hierarchy(None, *depth).len(),
        MixedScenario::HierarchyRoot { root, depth } => graph.hierarchy(Some(root), *depth).len(),
        MixedScenario::SymbolLookup { name } => graph.find_symbol_by_name(name).len(),
        MixedScenario::SignatureSearch { text, kind } => graph
            .signature_search(&SignatureQuery {
                text: text.clone(),
                kind: *kind,
                visibility: None,
                attribute: None,
            })
            .len(),
        MixedScenario::BodySearch { text, hit_kind } => graph
            .body_search(&BodySearchQuery {
                text: text.clone(),
                owner_kind: None,
                hit_kind: *hit_kind,
            })
            .len(),
        MixedScenario::ReferenceSearch { text } => graph.reference_search(text).len(),
        MixedScenario::ReferencesToSymbol { symbol } => graph.references_to_symbol(symbol).len(),
        MixedScenario::Callees { symbol } => graph.callees(symbol).len(),
        MixedScenario::Callers { symbol } => graph.callers(symbol).len(),
        MixedScenario::CallChain { from, to } => graph
            .call_chain(from, to, 8)
            .map(|chain| chain.len())
            .unwrap_or_default(),
    }
}

fn run_refresh_probe(repo: &Path) -> Result<RefreshProbeReport> {
    let source_snapshot = WorkspaceCrawler::new(CrawlOptions::default()).crawl(repo)?;
    let temp_root = temp_dir("squeezy-refresh-probe")?;
    let mut copied = Vec::new();
    for record in source_snapshot
        .files
        .iter()
        .filter(|record| record.language == LanguageKind::Rust)
        .take(250)
    {
        let dest = temp_root.join(&record.relative_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&record.path, &dest)?;
        copied.push(dest);
    }

    let mut manager = GraphManager::open_with_config(
        &temp_root,
        RefreshConfig {
            debounce: std::time::Duration::from_millis(0),
            idle_refresh_interval: std::time::Duration::from_millis(0),
            per_tool_refresh_budget: std::time::Duration::from_secs(10),
        },
    )?;

    let edits = copied.iter().take(2).cloned().collect::<Vec<_>>();
    for path in &edits {
        let mut text = fs::read_to_string(path)?;
        text.push_str("\n// squeezy refresh benchmark edit\n");
        fs::write(path, text)?;
        manager.record_changed_path(path.clone());
    }

    let refresh_started = Instant::now();
    let report = manager.refresh_before_query()?;
    let refresh_ms = refresh_started.elapsed().as_millis();
    fs::remove_dir_all(&temp_root)?;

    Ok(RefreshProbeReport {
        copied_rust_files: copied.len(),
        edited_files: edits.len(),
        refresh_ms,
        reparsed_files: report.reparsed_files,
        changed_files: report.changed_files.len(),
        budget_exhausted: report.budget_exhausted,
    })
}

fn flatten_hierarchy(graph: &SemanticGraph) -> Vec<String> {
    fn visit(node: &squeezy_graph::HierarchyNode, out: &mut Vec<String>) {
        out.push(format!("{:?}:{}", node.kind, node.name));
        for child in &node.children {
            visit(child, out);
        }
    }

    let mut out = Vec::new();
    for node in graph.hierarchy(None, 16) {
        visit(&node, &mut out);
    }
    out
}

fn parse_symbol_kind(value: &str) -> Result<SymbolKind> {
    match value {
        "Class" => Ok(SymbolKind::Class),
        "Crate" => Ok(SymbolKind::Crate),
        "File" => Ok(SymbolKind::File),
        "Module" => Ok(SymbolKind::Module),
        "Struct" => Ok(SymbolKind::Struct),
        "Enum" => Ok(SymbolKind::Enum),
        "Union" => Ok(SymbolKind::Union),
        "Trait" => Ok(SymbolKind::Trait),
        "Impl" => Ok(SymbolKind::Impl),
        "Function" => Ok(SymbolKind::Function),
        "Method" => Ok(SymbolKind::Method),
        "Const" => Ok(SymbolKind::Const),
        "Static" => Ok(SymbolKind::Static),
        "TypeAlias" => Ok(SymbolKind::TypeAlias),
        "Macro" => Ok(SymbolKind::Macro),
        "Test" => Ok(SymbolKind::Test),
        other => Err(SqueezyError::Graph(format!("unknown symbol kind {other}"))),
    }
}

fn time_cargo_check(fixture: &Path) -> Result<u128> {
    let manifest = fixture.join("Cargo.toml");
    let mut command = Command::new("cargo");
    if manifest.exists() {
        command
            .arg("check")
            .arg("--manifest-path")
            .arg(manifest)
            .arg("--quiet");
    } else {
        command.arg("check").arg("--quiet").current_dir(fixture);
    }

    let started = Instant::now();
    let status = command.status()?;
    let elapsed = started.elapsed().as_millis();
    if status.success() {
        Ok(elapsed)
    } else {
        Err(SqueezyError::Graph(format!(
            "compiler validation failed with {status}"
        )))
    }
}

fn time_python_ast_oracle(fixture: &Path) -> Result<u128> {
    let started = Instant::now();
    let _ = collect_python_ast_symbol_scan(fixture)?;
    Ok(started.elapsed().as_millis())
}

fn time_cargo_check_optional(repo: &Path) -> (Option<u128>, String) {
    match time_cargo_check(repo) {
        Ok(ms) => (Some(ms), "cargo check succeeded".to_string()),
        Err(err) => (None, format!("cargo check failed: {err}")),
    }
}

fn time_rust_analyzer(repo: &Path) -> (Option<u128>, String) {
    let started = Instant::now();
    let Some(mut command) = rust_analyzer_command() else {
        return (None, "rust-analyzer not found".to_string());
    };
    let output = command
        .arg("analysis-stats")
        .arg("--run-all-ide-things")
        .arg(repo)
        .output();
    match output {
        Ok(output) if output.status.success() => (
            Some(started.elapsed().as_millis()),
            "rust-analyzer analysis-stats --run-all-ide-things succeeded".to_string(),
        ),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default();
            (
                None,
                format!(
                    "rust-analyzer analysis-stats failed with {}{}",
                    output.status,
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", truncate(detail, 240))
                    }
                ),
            )
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            (None, "rust-analyzer not found".to_string())
        }
        Err(err) => (None, format!("rust-analyzer failed to start: {err}")),
    }
}

struct RustAnalyzerLsp {
    root: PathBuf,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    opened: BTreeSet<String>,
}

impl RustAnalyzerLsp {
    fn start(root: &Path) -> Result<Self> {
        let Some(program) = rust_analyzer_program() else {
            return Err(SqueezyError::Graph("rust-analyzer not found".to_string()));
        };
        let root = fs::canonicalize(root)?;
        let mut child = Command::new(program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| SqueezyError::Graph("failed to open rust-analyzer stdin".to_string()))?;
        let stdout = child.stdout.take().ok_or_else(|| {
            SqueezyError::Graph("failed to open rust-analyzer stdout".to_string())
        })?;
        let mut client = Self {
            root: root.clone(),
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            opened: BTreeSet::new(),
        };
        let root_uri = path_to_file_uri(&root)?;
        client.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "workspaceFolders": [{
                    "uri": root_uri,
                    "name": root.file_name().and_then(|name| name.to_str()).unwrap_or("workspace"),
                }],
                "capabilities": {
                    "textDocument": {
                        "definition": {},
                        "references": {}
                    }
                }
            }),
        )?;
        client.notify("initialized", json!({}))?;
        std::thread::sleep(std::time::Duration::from_millis(750));
        Ok(client)
    }

    fn definition(&mut self, uri: &str, position: LspPosition) -> Result<Vec<LocationKey>> {
        let value = self.request(
            "textDocument/definition",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": position.line, "character": position.character}
            }),
        )?;
        parse_lsp_locations(&value, &self.root)
    }

    fn references(&mut self, uri: &str, position: LspPosition) -> Result<Vec<LocationKey>> {
        let value = self.request(
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": position.line, "character": position.character},
                "context": {"includeDeclaration": false}
            }),
        )?;
        parse_lsp_locations(&value, &self.root)
    }

    fn did_open(&mut self, uri: &str, path: &Path) -> Result<()> {
        if !self.opened.insert(uri.to_string()) {
            return Ok(());
        }
        let text = fs::read_to_string(path)?;
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "rust",
                    "version": 1,
                    "text": text
                }
            }),
        )
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let mut last_error = None;
        for _ in 0..4 {
            match self.request_once(method, params.clone()) {
                Ok(value) => return Ok(value),
                Err(err) if err.to_string().contains("content modified") => {
                    last_error = Some(err);
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            SqueezyError::Graph(format!("LSP request {method} failed after retries"))
        }))
    }

    fn request_once(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;

        loop {
            let message = self.read_message()?;
            if message.get("id").and_then(Value::as_i64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(SqueezyError::Graph(format!(
                    "LSP request {method} failed: {error}"
                )));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))
    }

    fn write_message(&mut self, value: &Value) -> Result<()> {
        let body = serde_json::to_vec(value).map_err(|err| {
            SqueezyError::Graph(format!("failed to serialize LSP message: {err}"))
        })?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
        self.stdin.write_all(&body)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<Value> {
        let mut content_length = None;
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line)?;
            if read == 0 {
                return Err(SqueezyError::Graph(
                    "rust-analyzer LSP closed stdout".to_string(),
                ));
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some(raw) = trimmed.strip_prefix("Content-Length:") {
                content_length = Some(raw.trim().parse::<usize>().map_err(|err| {
                    SqueezyError::Graph(format!("invalid LSP Content-Length {raw}: {err}"))
                })?);
            }
        }

        let len = content_length
            .ok_or_else(|| SqueezyError::Graph("missing LSP Content-Length".to_string()))?;
        let mut body = vec![0; len];
        self.stdout.read_exact(&mut body)?;
        serde_json::from_slice(&body)
            .map_err(|err| SqueezyError::Graph(format!("invalid LSP JSON response: {err}")))
    }
}

impl Drop for RustAnalyzerLsp {
    fn drop(&mut self) {
        let _ = self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": "shutdown",
            "params": null
        }));
        let _ = self.write_message(&json!({
            "jsonrpc": "2.0",
            "method": "exit",
            "params": null
        }));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_lsp_locations(value: &Value, root: &Path) -> Result<Vec<LocationKey>> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    if let Some(items) = value.as_array() {
        return items
            .iter()
            .map(|item| parse_lsp_location(item, root))
            .collect();
    }
    parse_lsp_location(value, root).map(|location| vec![location])
}

fn parse_lsp_location(value: &Value, root: &Path) -> Result<LocationKey> {
    let uri = value
        .get("uri")
        .or_else(|| value.get("targetUri"))
        .and_then(Value::as_str)
        .ok_or_else(|| SqueezyError::Graph(format!("LSP location missing uri: {value}")))?;
    let range = value
        .get("range")
        .or_else(|| value.get("targetSelectionRange"))
        .or_else(|| value.get("targetRange"))
        .ok_or_else(|| SqueezyError::Graph(format!("LSP location missing range: {value}")))?;
    let start = range
        .get("start")
        .ok_or_else(|| SqueezyError::Graph(format!("LSP range missing start: {range}")))?;
    let line = start
        .get("line")
        .and_then(Value::as_u64)
        .ok_or_else(|| SqueezyError::Graph(format!("LSP range start missing line: {start}")))?
        as u32;
    let character = start
        .get("character")
        .and_then(Value::as_u64)
        .ok_or_else(|| SqueezyError::Graph(format!("LSP range start missing character: {start}")))?
        as u32;
    let path = file_uri_to_path(uri)?;
    Ok(LocationKey {
        file: location_file_key(root, &path),
        line,
        character,
    })
}

fn location_file_key(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .map(|relative| relative.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

fn path_to_file_uri(path: &Path) -> Result<String> {
    let path = fs::canonicalize(path)?;
    let raw = path.to_string_lossy();
    Ok(format!("file://{}", percent_encode_path(&raw)))
}

fn file_uri_to_path(uri: &str) -> Result<PathBuf> {
    let raw = uri
        .strip_prefix("file://")
        .ok_or_else(|| SqueezyError::Graph(format!("unsupported non-file URI {uri}")))?;
    Ok(PathBuf::from(percent_decode(raw)?))
}

fn percent_encode_path(path: &str) -> String {
    let mut out = String::new();
    for byte in path.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn percent_decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                .map_err(|err| SqueezyError::Graph(format!("invalid URI escape: {err}")))?;
            out.push(
                u8::from_str_radix(hex, 16).map_err(|err| {
                    SqueezyError::Graph(format!("invalid URI escape %{hex}: {err}"))
                })?,
            );
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out)
        .map_err(|err| SqueezyError::Graph(format!("invalid UTF-8 file URI: {err}")))
}

fn byte_to_lsp_position(source: &str, byte: usize) -> LspPosition {
    let byte = byte.min(source.len());
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (index, ch) in source.char_indices() {
        if index >= byte {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = index + ch.len_utf8();
        }
    }
    let character = source
        .get(line_start..byte)
        .unwrap_or_default()
        .encode_utf16()
        .count() as u32;
    LspPosition { line, character }
}

fn line_char_to_byte(source: &str, line: u32, character: u32) -> Option<usize> {
    let mut current_line = 0u32;
    let mut line_start = 0usize;
    for (index, ch) in source.char_indices() {
        if current_line == line {
            break;
        }
        if ch == '\n' {
            current_line += 1;
            line_start = index + ch.len_utf8();
        }
    }
    if current_line != line {
        return None;
    }

    let mut utf16 = 0u32;
    for (offset, ch) in source[line_start..].char_indices() {
        if ch == '\n' {
            break;
        }
        if utf16 == character {
            return Some(line_start + offset);
        }
        utf16 += ch.len_utf16() as u32;
        if utf16 > character {
            return Some(line_start + offset);
        }
    }
    Some(
        line_start
            + source[line_start..]
                .lines()
                .next()
                .unwrap_or_default()
                .len(),
    )
}

fn rust_analyzer_command() -> Option<Command> {
    rust_analyzer_program().map(Command::new)
}

fn rust_analyzer_program() -> Option<String> {
    if command_exists("rust-analyzer") {
        return Some("rust-analyzer".to_string());
    }
    let output = Command::new("rustup")
        .arg("which")
        .arg("rust-analyzer")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

fn command_exists(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect::<String>()
}

fn write_report(path: &Path, report: &BenchmarkReport) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(report)
        .map_err(|err| SqueezyError::Graph(format!("failed to serialize report: {err}")))?;
    fs::write(path, format!("{text}\n"))?;
    Ok(())
}

fn print_summary(report: &BenchmarkReport) {
    println!("semantic graph benchmark");
    println!("language: {}", report.language);
    println!("fixture: {}", report.fixture);
    println!(
        "validation: {} ({}ms)",
        report.validation_status, report.validation_ms
    );
    println!("squeezy_total_ms: {}", report.squeezy_total_ms);
    println!(
        "faster_than_validation: {}",
        report.faster_than_validation
    );
    print_accuracy_summary("fixture", &report.accuracy);
    print_navigation_summary("fixture", &report.accuracy.navigation);
    if let Some(python) = &report.python_oracle {
        println!(
            "python_oracle_symbol_accuracy: tp={} fp={} fn={} precision={} recall={} oracle_symbols={} squeezy_symbols={} oracle={} oracle_unparseable={}",
            python.symbols.true_positive,
            python.symbols.false_positive,
            python.symbols.false_negative,
            python.symbols.precision,
            python.symbols.recall,
            python.symbols.rust_analyzer_total,
            python.symbols.squeezy_total,
            format!("{}ms", python.oracle_ms),
            python.oracle_unparseable_files
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

fn print_accuracy_summary(label: &str, accuracy: &AccuracyReport) {
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

fn print_navigation_summary(label: &str, navigation: &NavigationAccuracyReport) {
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

fn enforce_gates(report: &BenchmarkReport, no_speed_gate: bool) -> Result<()> {
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

fn increment(counts: &mut BTreeMap<String, usize>, key: &str) {
    *counts.entry(key.to_string()).or_default() += 1;
}

fn temp_dir(prefix: &str) -> Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| SqueezyError::Graph(format!("clock error: {err}")))?
        .as_nanos();
    let path = env::temp_dir().join(format!("{prefix}-{nonce}"));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn required<'a>(value: &'a Option<String>, name: &str) -> Result<&'a str> {
    value
        .as_deref()
        .ok_or_else(|| SqueezyError::Graph(format!("query missing required {name}")))
}

struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_usize(&mut self, upper_bound: usize) -> usize {
        if upper_bound == 0 {
            return 0;
        }
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (self.state as usize) % upper_bound
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BenchmarkLanguage {
    Java,
    Python,
    Rust,
}

impl BenchmarkLanguage {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "java" => Ok(Self::Java),
            "python" => Ok(Self::Python),
            "rust" => Ok(Self::Rust),
            other => Err(SqueezyError::Graph(format!(
                "unknown benchmark language {other}"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Java => "java",
            Self::Python => "python",
            Self::Rust => "rust",
        }
    }
}

struct Args {
    language: BenchmarkLanguage,
    fixture: PathBuf,
    spec: PathBuf,
    report: PathBuf,
    mixed_repo: Option<PathBuf>,
    mixed_iterations: usize,
    ra_lsp_probes: usize,
    no_speed_gate: bool,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut fixture = None;
        let mut language = BenchmarkLanguage::Rust;
        let mut spec = None;
        let mut report = None;
        let mut mixed_repo = None;
        let mut mixed_iterations = 0;
        let mut ra_lsp_probes = 25;
        let mut no_speed_gate = false;
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--language" => {
                    let raw = args.next().ok_or_else(|| {
                        SqueezyError::Graph("missing --language value".to_string())
                    })?;
                    language = BenchmarkLanguage::parse(&raw)?;
                }
                "--fixture" => fixture = args.next().map(PathBuf::from),
                "--spec" => spec = args.next().map(PathBuf::from),
                "--report" => report = args.next().map(PathBuf::from),
                "--mixed-repo" => mixed_repo = args.next().map(PathBuf::from),
                "--no-speed-gate" => no_speed_gate = true,
                "--mixed-iterations" => {
                    let raw = args.next().ok_or_else(|| {
                        SqueezyError::Graph("missing --mixed-iterations value".to_string())
                    })?;
                    mixed_iterations = raw.parse().map_err(|err| {
                        SqueezyError::Graph(format!("invalid --mixed-iterations {raw}: {err}"))
                    })?;
                }
                "--ra-lsp-probes" => {
                    let raw = args.next().ok_or_else(|| {
                        SqueezyError::Graph("missing --ra-lsp-probes value".to_string())
                    })?;
                    ra_lsp_probes = raw.parse().map_err(|err| {
                        SqueezyError::Graph(format!("invalid --ra-lsp-probes {raw}: {err}"))
                    })?;
                }
                "--help" | "-h" => {
                    println!(
                        "usage: squeezy-graph-bench [--language rust|python|java] --fixture <path> --spec <path> --report <path> [--mixed-repo <path>] [--mixed-iterations <n, 0=all>] [--ra-lsp-probes <n, default=25, 0=off>] [--no-speed-gate]"
                    );
                    std::process::exit(0);
                }
                other => {
                    return Err(SqueezyError::Graph(format!("unknown argument {other}")));
                }
            }
        }

        Ok(Self {
            language,
            fixture: fixture.ok_or_else(|| SqueezyError::Graph("missing --fixture".to_string()))?,
            spec: spec.ok_or_else(|| SqueezyError::Graph("missing --spec".to_string()))?,
            report: report.ok_or_else(|| SqueezyError::Graph("missing --report".to_string()))?,
            mixed_repo,
            mixed_iterations,
            ra_lsp_probes,
            no_speed_gate,
        })
    }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
