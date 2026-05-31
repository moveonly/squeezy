use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    time::Instant,
};

use squeezy_core::{Confidence, Result, SqueezyError, SymbolKind};
use squeezy_graph::{BodySearchQuery, SemanticGraph, SignatureQuery};
use squeezy_parse::{BodyHitKind, LanguageParser, ParsedFile};
use squeezy_workspace::{CrawlOptions, IndexCoverage, WorkspaceCrawler, WorkspaceSnapshot};

use crate::{
    accuracy::ratio,
    report::{
        BuildPhaseReport, FallbackQualityReport, FallbackReasonReport, GrepBaselineMode,
        GrepBaselineSpec, QueryBaselineReport, QueryBaselineStatus, QueryReport, QuerySpec,
    },
};

pub(crate) struct GraphBuildOutput {
    pub(crate) graph: SemanticGraph,
    pub(crate) phases: BuildPhaseReport,
    pub(crate) coverage: IndexCoverage,
    pub(crate) unsupported_files: usize,
    pub(crate) unsupported_file_samples: Vec<String>,
    pub(crate) snapshot: WorkspaceSnapshot,
}

pub(crate) fn build_graph(root: &Path) -> Result<GraphBuildOutput> {
    let total_started = Instant::now();
    let crawl_started = Instant::now();
    let snapshot = WorkspaceCrawler::new(CrawlOptions::default()).crawl(root)?;
    let coverage = snapshot.coverage.clone();
    let unsupported_files = snapshot.unsupported.len();
    let unsupported_file_samples = snapshot
        .unsupported
        .iter()
        .take(10)
        .map(|file| file.relative_path.clone())
        .collect();
    let snapshot_for_grep = snapshot.clone();
    let crawl_ms = crawl_started.elapsed().as_millis();

    let parse_started = Instant::now();
    let mut parser = LanguageParser::new()?;
    let (parsed, _) = parser.parse_records(&snapshot.files)?;
    let parse_ms = parse_started.elapsed().as_millis();

    let declaration_started = Instant::now();
    let declaration_graph = SemanticGraph::from_parsed(declaration_only_parsed(&parsed));
    let declaration_graph_ms = declaration_started.elapsed().as_millis();
    drop(declaration_graph);

    let full_graph_started = Instant::now();
    let graph = SemanticGraph::from_parsed(parsed);
    let full_graph_ms = full_graph_started.elapsed().as_millis();

    Ok(GraphBuildOutput {
        graph,
        phases: BuildPhaseReport {
            crawl_ms,
            parse_ms,
            declaration_graph_ms,
            full_graph_ms,
            total_ms: total_started.elapsed().as_millis(),
        },
        coverage,
        unsupported_files,
        unsupported_file_samples,
        snapshot: snapshot_for_grep,
    })
}

pub(crate) fn fallback_quality_report(
    coverage: &IndexCoverage,
    unsupported_files: usize,
    unsupported_file_samples: Vec<String>,
    graph: &SemanticGraph,
) -> FallbackQualityReport {
    let coverage_reasons = coverage
        .reasons
        .iter()
        .map(|(reason, item)| {
            (
                reason.clone(),
                FallbackReasonReport {
                    files: item.files,
                    dirs: item.dirs,
                    bytes: item.bytes,
                    samples: item.samples.clone(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();

    let mut edge_confidence = BTreeMap::new();
    for edge in graph.edges() {
        *edge_confidence
            .entry(format!("{:?}", edge.confidence))
            .or_insert(0) += 1;
    }
    let low_confidence_edges = graph
        .edges()
        .iter()
        .filter(|edge| {
            !matches!(
                edge.confidence,
                Confidence::ExactSyntax | Confidence::ImportResolved
            )
        })
        .count();
    let total_edges = graph.edges().len();

    FallbackQualityReport {
        unsupported_files,
        unsupported_file_samples,
        excluded_files: coverage.skipped_files,
        excluded_dirs: coverage.skipped_dirs,
        excluded_bytes: coverage.skipped_bytes,
        coverage_reasons,
        edge_confidence,
        low_confidence_edges,
        fallback_rate: ratio(
            unsupported_files + coverage.skipped_files + low_confidence_edges,
            unsupported_files + coverage.skipped_files + total_edges,
        ),
    }
}

pub(crate) fn declaration_only_parsed(parsed: &[ParsedFile]) -> Vec<ParsedFile> {
    parsed
        .iter()
        .cloned()
        .map(|mut file| {
            file.imports.clear();
            file.calls.clear();
            file.references.clear();
            file.body_hits.clear();
            file
        })
        .collect()
}

pub(crate) fn run_query(
    snapshot: &WorkspaceSnapshot,
    graph: &SemanticGraph,
    query: &QuerySpec,
    fallback_quality: &FallbackQualityReport,
) -> Result<QueryReport> {
    let actual = match query.kind.as_str() {
        "fallback_quality" => fallback_quality_lines(fallback_quality),
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
        "dotnet_project_facts" => graph
            .dotnet_project_facts()
            .iter()
            .map(|fact| format!("{}:{}:{}", fact.provider, fact.kind, fact.value))
            .collect(),
        "kotlin_project_facts" => graph
            .kotlin_project_facts()
            .iter()
            .map(|fact| format!("{}:{}:{}", fact.provider, fact.kind, fact.value))
            .collect(),
        "edges" => graph
            .edges()
            .iter()
            .filter_map(|edge| {
                let from = graph.symbols.get(&edge.from)?;
                let to = edge
                    .to
                    .as_ref()
                    .and_then(|id| graph.symbols.get(id))
                    .map(|symbol| symbol.name.as_str())
                    .unwrap_or("<unresolved>");
                Some(format!(
                    "{:?}:{}->{}:{}:{:?}",
                    edge.kind, from.name, to, edge.target_text, edge.confidence
                ))
            })
            .collect(),
        "references_to_symbol" => {
            let to = required(&query.to, "to")?;
            let symbol = benchmark_symbol_by_name(graph, to)
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
    let baseline = run_grep_baseline(snapshot, query)?;

    Ok(QueryReport {
        id: query.id.clone(),
        kind: query.kind.clone(),
        expected_contains: query.expected_contains.clone(),
        actual: actual.into_iter().collect(),
        missing,
        extras,
        documented_misses: query.documented_misses.clone(),
        baseline,
    })
}

fn fallback_quality_lines(fallback: &FallbackQualityReport) -> Vec<String> {
    let mut lines = Vec::new();
    for (reason, coverage) in &fallback.coverage_reasons {
        lines.push(reason.clone());
        lines.push(format!("{reason}:files={}", coverage.files));
        lines.push(format!("{reason}:dirs={}", coverage.dirs));
    }
    for (confidence, count) in &fallback.edge_confidence {
        lines.push(format!("edge_confidence:{confidence}={count}"));
    }
    if fallback.unsupported_files > 0 {
        lines.push("unsupported".to_string());
        lines.push(format!("unsupported:files={}", fallback.unsupported_files));
    }
    lines
}

fn run_grep_baseline(
    snapshot: &WorkspaceSnapshot,
    query: &QuerySpec,
) -> Result<QueryBaselineReport> {
    let (baseline, semantic_relation_supported) = grep_baseline_for_query(query);
    if matches!(baseline.mode, GrepBaselineMode::Unsupported) {
        return Ok(QueryBaselineReport {
            status: QueryBaselineStatus::Unsupported,
            status_detail: baseline
                .unsupported_reason
                .unwrap_or_else(|| "grep baseline cannot model this semantic query".to_string()),
            pattern: baseline.pattern,
            include: baseline.include,
            files_scanned: 0,
            bytes_read: 0,
            matches_returned: 0,
            actual: Vec::new(),
            semantic_relation_supported,
        });
    }

    let Some(pattern) = baseline
        .pattern
        .clone()
        .filter(|pattern| !pattern.is_empty())
    else {
        return Ok(QueryBaselineReport {
            status: QueryBaselineStatus::Skipped,
            status_detail: "skipped: no grep pattern available".to_string(),
            pattern: baseline.pattern,
            include: baseline.include,
            files_scanned: 0,
            bytes_read: 0,
            matches_returned: 0,
            actual: Vec::new(),
            semantic_relation_supported,
        });
    };

    let mut files_scanned = 0usize;
    let mut bytes_read = 0u64;
    let mut matches = Vec::new();
    let mut matched_paths = BTreeSet::new();
    for file in &snapshot.files {
        if !baseline.include.is_empty()
            && !baseline
                .include
                .iter()
                .any(|pattern| baseline_path_matches(pattern, &file.relative_path))
        {
            continue;
        }
        files_scanned += 1;
        let content = match fs::read_to_string(&file.path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        bytes_read += content.len() as u64;
        for (line_index, line) in content.lines().enumerate() {
            if line.contains(&pattern) {
                matched_paths.insert(file.relative_path.clone());
                matches.push(format!(
                    "{}:{}:{}",
                    file.relative_path,
                    line_index + 1,
                    line
                ));
            }
        }
    }

    let raw_match_count = matches.len();
    let actual = match baseline.mode {
        GrepBaselineMode::Paths => matched_paths.into_iter().collect(),
        GrepBaselineMode::Count => vec![matches.len().to_string()],
        GrepBaselineMode::FirstLine => matches.into_iter().take(1).collect(),
        GrepBaselineMode::Unsupported => Vec::new(),
    };

    Ok(QueryBaselineReport {
        status: QueryBaselineStatus::Ran,
        status_detail: if semantic_relation_supported {
            "grep baseline ran".to_string()
        } else {
            "grep baseline ran; semantic relation not modeled".to_string()
        },
        pattern: Some(pattern),
        include: baseline.include,
        files_scanned,
        bytes_read,
        matches_returned: raw_match_count,
        actual,
        semantic_relation_supported,
    })
}

fn grep_baseline_for_query(query: &QuerySpec) -> (GrepBaselineSpec, bool) {
    if let Some(baseline) = &query.baseline {
        let semantic = !matches!(baseline.mode, GrepBaselineMode::Unsupported);
        return (baseline.clone(), semantic);
    }
    let semantic_relation_supported = matches!(
        query.kind.as_str(),
        "signature_search" | "body_search" | "reference_search" | "hierarchy_contains"
    );
    let pattern = if !query.text.is_empty() {
        Some(query.text.clone())
    } else {
        query
            .to
            .clone()
            .or_else(|| query.from.clone())
            .or_else(|| query.expected_contains.first().cloned())
    };
    (
        GrepBaselineSpec {
            pattern,
            include: Vec::new(),
            mode: GrepBaselineMode::Paths,
            unsupported_reason: None,
        },
        semantic_relation_supported,
    )
}

fn baseline_path_matches(pattern: &str, path: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        return path.ends_with(&format!(".{suffix}"));
    }
    let normalized = pattern.trim_start_matches('/');
    if normalized.is_empty() {
        return true;
    }
    if path == normalized || path == normalized.trim_end_matches('/') {
        return true;
    }
    let dir_prefix = if normalized.ends_with('/') {
        normalized.to_string()
    } else {
        format!("{normalized}/")
    };
    path.starts_with(&dir_prefix)
}

pub(crate) fn benchmark_symbol_by_name(
    graph: &SemanticGraph,
    name: &str,
) -> Option<squeezy_graph::GraphSymbol> {
    let mut symbols = graph.find_symbol_by_name(name);
    symbols.sort_by(|left, right| {
        right
            .body_span
            .is_some()
            .cmp(&left.body_span.is_some())
            .then_with(|| left.id.0.cmp(&right.id.0))
    });
    symbols.into_iter().next()
}

pub(crate) fn flatten_hierarchy(graph: &SemanticGraph) -> Vec<String> {
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

pub(crate) fn parse_symbol_kind(value: &str) -> Result<SymbolKind> {
    match value {
        "Class" => Ok(SymbolKind::Class),
        "Crate" => Ok(SymbolKind::Crate),
        "File" => Ok(SymbolKind::File),
        "Interface" => Ok(SymbolKind::Interface),
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

fn required<'a>(value: &'a Option<String>, name: &str) -> Result<&'a str> {
    value
        .as_deref()
        .ok_or_else(|| SqueezyError::Graph(format!("query missing required {name}")))
}

#[cfg(test)]
#[path = "execution_tests.rs"]
mod tests;
