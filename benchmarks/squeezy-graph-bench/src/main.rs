use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
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
    fixture: String,
    spec: String,
    rustc_check_ms: u128,
    squeezy_build_ms: u128,
    squeezy_query_ms: u128,
    squeezy_total_ms: u128,
    faster_than_rustc_check: bool,
    graph: GraphReport,
    accuracy: AccuracyReport,
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

    let rustc_check_ms = time_cargo_check(&args.fixture)?;

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
    let accuracy = collect_accuracy(&graph);

    let mixed_workload = args
        .mixed_repo
        .as_ref()
        .map(|repo| run_mixed_workload(repo, args.mixed_iterations))
        .transpose()?;

    let stats = graph.stats();
    let report = BenchmarkReport {
        fixture: args.fixture.display().to_string(),
        spec: args.spec.display().to_string(),
        rustc_check_ms,
        squeezy_build_ms,
        squeezy_query_ms,
        squeezy_total_ms,
        faster_than_rustc_check: squeezy_total_ms < rustc_check_ms,
        graph: GraphReport {
            files: stats.files,
            symbols: stats.symbols,
            edges: stats.edges,
            body_hits: stats.body_hits,
            references: stats.references,
            calls: stats.calls,
        },
        accuracy,
        queries: query_reports,
        mixed_workload,
    };

    write_report(&args.report, &report)?;
    print_summary(&report);
    enforce_gates(&report)
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
                attribute: None,
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
        "call_chain" => {
            let from = required(&query.from, "from")?;
            let to = required(&query.to, "to")?;
            let from_symbol = graph
                .find_symbol_by_name(from)
                .into_iter()
                .next()
                .ok_or_else(|| SqueezyError::Graph(format!("missing symbol {from}")))?;
            let to_symbol = graph
                .find_symbol_by_name(to)
                .into_iter()
                .next()
                .ok_or_else(|| SqueezyError::Graph(format!("missing symbol {to}")))?;
            graph
                .call_chain(&from_symbol.id, &to_symbol.id, 8)
                .map(|chain| {
                    vec![
                        chain
                            .iter()
                            .filter_map(|id| graph.symbols.get(id))
                            .map(|symbol| symbol.name.clone())
                            .collect::<Vec<_>>()
                            .join(" -> "),
                    ]
                })
                .unwrap_or_default()
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

fn run_mixed_workload(repo: &Path, requested_scenarios: usize) -> Result<MixedWorkloadReport> {
    let (compiler_check_ms, compiler_check_status) = time_cargo_check_optional(repo);
    let (rust_analyzer_ms, rust_analyzer_status) = time_rust_analyzer(repo);

    let build_started = Instant::now();
    let graph = build_graph(repo)?;
    let squeezy_build_ms = build_started.elapsed().as_millis();
    let accuracy = collect_accuracy(&graph);

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

fn collect_accuracy(graph: &SemanticGraph) -> AccuracyReport {
    let squeezy_symbols = collect_squeezy_symbol_scan(graph);
    let started = Instant::now();
    let (rust_analyzer_symbols, status) = collect_rust_analyzer_symbol_scan(graph);
    let rust_analyzer_symbols_ms = if status.starts_with("rust-analyzer symbols succeeded") {
        Some(started.elapsed().as_millis())
    } else {
        None
    };
    let symbols = compare_symbol_sets(&squeezy_symbols, &rust_analyzer_symbols);

    AccuracyReport {
        rust_analyzer_symbols_ms,
        rust_analyzer_symbol_status: status,
        symbols,
        limitations: vec![
            "Symbol TP/FP/FN compares declaration families both engines expose; raw rust-analyzer locals and fields are counted as excluded, not silently compared.".to_string(),
            "Call-target and reference TP/FP/FN are not yet compared because rust-analyzer CLI search failed locally and SCIP/rustc-HIR oracle parsing is future work.".to_string(),
            "Macro-generated items, proc macros, cfg matrices, trait dispatch, deref/autoref method resolution, and external crate/stdlib references remain documented lower-confidence areas.".to_string(),
        ],
    }
}

fn collect_squeezy_symbol_scan(graph: &SemanticGraph) -> SymbolScan {
    let mut scan = SymbolScan::default();
    for symbol in graph.symbols.values() {
        scan.raw_total += 1;
        match normalize_squeezy_kind(symbol.kind) {
            Some(kind) => {
                let Some(file) = graph.files.get(&symbol.file_id) else {
                    increment(&mut scan.excluded_by_kind, "MissingFile");
                    continue;
                };
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
    println!("fixture: {}", report.fixture);
    println!("rustc_check_ms: {}", report.rustc_check_ms);
    println!("squeezy_total_ms: {}", report.squeezy_total_ms);
    println!(
        "faster_than_rustc_check: {}",
        report.faster_than_rustc_check
    );
    print_accuracy_summary("fixture", &report.accuracy);
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

fn enforce_gates(report: &BenchmarkReport) -> Result<()> {
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

    if !report.faster_than_rustc_check {
        return Err(SqueezyError::Graph(format!(
            "Squeezy graph was not faster than rustc validation: {}ms >= {}ms",
            report.squeezy_total_ms, report.rustc_check_ms
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

struct Args {
    fixture: PathBuf,
    spec: PathBuf,
    report: PathBuf,
    mixed_repo: Option<PathBuf>,
    mixed_iterations: usize,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut fixture = None;
        let mut spec = None;
        let mut report = None;
        let mut mixed_repo = None;
        let mut mixed_iterations = 0;
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--fixture" => fixture = args.next().map(PathBuf::from),
                "--spec" => spec = args.next().map(PathBuf::from),
                "--report" => report = args.next().map(PathBuf::from),
                "--mixed-repo" => mixed_repo = args.next().map(PathBuf::from),
                "--mixed-iterations" => {
                    let raw = args.next().ok_or_else(|| {
                        SqueezyError::Graph("missing --mixed-iterations value".to_string())
                    })?;
                    mixed_iterations = raw.parse().map_err(|err| {
                        SqueezyError::Graph(format!("invalid --mixed-iterations {raw}: {err}"))
                    })?;
                }
                "--help" | "-h" => {
                    println!(
                        "usage: squeezy-graph-bench --fixture <path> --spec <path> --report <path> [--mixed-repo <path>] [--mixed-iterations <n, 0=all>]"
                    );
                    std::process::exit(0);
                }
                other => {
                    return Err(SqueezyError::Graph(format!("unknown argument {other}")));
                }
            }
        }

        Ok(Self {
            fixture: fixture.ok_or_else(|| SqueezyError::Graph("missing --fixture".to_string()))?,
            spec: spec.ok_or_else(|| SqueezyError::Graph("missing --spec".to_string()))?,
            report: report.ok_or_else(|| SqueezyError::Graph("missing --report".to_string()))?,
            mixed_repo,
            mixed_iterations,
        })
    }
}
