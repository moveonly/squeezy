use std::{collections::BTreeSet, path::Path, time::Instant};

use squeezy_core::{Result, SqueezyError, SymbolKind};
use squeezy_graph::{BodySearchQuery, SemanticGraph, SignatureQuery};
use squeezy_parse::{BodyHitKind, LanguageParser, ParsedFile};
use squeezy_workspace::{CrawlOptions, WorkspaceCrawler};

use crate::report::{BuildPhaseReport, QueryReport, QuerySpec};

pub(crate) struct GraphBuildOutput {
    pub(crate) graph: SemanticGraph,
    pub(crate) phases: BuildPhaseReport,
}

pub(crate) fn build_graph(root: &Path) -> Result<GraphBuildOutput> {
    let total_started = Instant::now();
    let crawl_started = Instant::now();
    let snapshot = WorkspaceCrawler::new(CrawlOptions::default()).crawl(root)?;
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
    })
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

pub(crate) fn run_query(graph: &SemanticGraph, query: &QuerySpec) -> Result<QueryReport> {
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
        "dotnet_project_facts" => graph
            .dotnet_project_facts()
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
