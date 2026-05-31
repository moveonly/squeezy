use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    time::Instant,
};

use squeezy_core::{EdgeKind, Result, SymbolKind};
use squeezy_graph::SemanticGraph;

use crate::{
    mixed::select_scenarios,
    oracles::{
        collect_rust_analyzer_symbol_scan, collect_squeezy_symbol_scan,
        rust_analyzer::{
            RustAnalyzerLsp, byte_to_lsp_position, line_char_to_byte, path_to_file_uri,
        },
    },
    report::*,
};

/// Generic LSP navigation client interface so the `compare_*_probes` helpers
/// can be shared between rust-analyzer (Rust corpus) and SourceKit-LSP
/// (Swift corpus). Implementations are responsible for the LSP transport;
/// the helpers below own the squeezy-vs-LSP TP/FP/FN bookkeeping.
pub(crate) trait LspNavigationClient {
    fn did_open(&mut self, uri: &str, path: &Path) -> Result<()>;
    fn definition(&mut self, uri: &str, position: LspPosition) -> Result<Vec<LocationKey>>;
    fn references(&mut self, uri: &str, position: LspPosition) -> Result<Vec<LocationKey>>;
}

pub(crate) fn collect_accuracy(
    root: &Path,
    graph: &SemanticGraph,
    ra_lsp_probes: usize,
) -> AccuracyReport {
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

pub(crate) fn empty_accuracy(status: &str) -> AccuracyReport {
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

pub(crate) fn compare_symbol_sets(
    squeezy: &SymbolScan,
    rust_analyzer: &SymbolScan,
) -> AccuracySetReport {
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
            "Interface".to_string(),
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

pub(crate) fn collect_navigation_accuracy(
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

pub(crate) fn navigation_limitations() -> Vec<String> {
    vec![
        "Definition probes compare Squeezy resolved call and macro edge targets with rust-analyzer LSP definitions for sampled call sites.".to_string(),
        "Reference probes compare Squeezy references_to_symbol results with rust-analyzer LSP references for sampled declarations, excluding declarations because the selected symbol already supplies the definition span.".to_string(),
        "Samples are deterministic and capped; increase --ra-lsp-probes for deeper local audits.".to_string(),
        "External dependency definitions are counted as rust-analyzer-only misses because Squeezy currently indexes workspace files only.".to_string(),
    ]
}

pub(crate) fn compare_definition_probes<C: LspNavigationClient>(
    root: &Path,
    graph: &SemanticGraph,
    client: &mut C,
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

pub(crate) fn compare_reference_probes<C: LspNavigationClient>(
    root: &Path,
    graph: &SemanticGraph,
    client: &mut C,
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

pub(crate) fn build_definition_probes(
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

pub(crate) fn build_reference_probes(
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

pub(crate) fn location_key_for_reference_hit(
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

pub(crate) fn probe_byte_for_edge(
    source: &str,
    start: usize,
    end: usize,
    target_text: &str,
) -> usize {
    let end = end.min(source.len());
    let start = start.min(end);
    let slice = source.get(start..end).unwrap_or_default();
    let needle = target_identifier(target_text);
    slice
        .rfind(&needle)
        .map(|index| start + index)
        .unwrap_or(start)
}

pub(crate) fn probe_byte_for_symbol(source: &str, start: usize, end: usize, name: &str) -> usize {
    let end = end.min(source.len());
    let start = start.min(end);
    let slice = source.get(start..end).unwrap_or_default();
    let needle = target_identifier(name);
    slice
        .find(&needle)
        .map(|index| start + index)
        .unwrap_or(start)
}

pub(crate) fn target_identifier(text: &str) -> String {
    let before_bang = text.split('!').next().unwrap_or(text);
    let before_call = before_bang.split('(').next().unwrap_or(before_bang);
    before_call
        .rsplit(|ch| ['.', ':', '<', '>', '&', ' ', '\t', '\n'].contains(&ch))
        .find(|part| !part.is_empty())
        .unwrap_or(before_call)
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .to_string()
}

pub(crate) fn location_matches_symbol(
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

pub(crate) fn render_locations(locations: &[LocationKey]) -> String {
    locations
        .iter()
        .take(5)
        .map(LocationKey::render)
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn push_example(examples: &mut Vec<String>, example: String) {
    if examples.len() < 20 {
        examples.push(example);
    }
}

pub(crate) fn merge_symbol_scan(target: &mut SymbolScan, source: SymbolScan) {
    target.raw_total += source.raw_total;
    target.skipped_non_utf8_files += source.skipped_non_utf8_files;
    for (key, count) in source.counts {
        *target.counts.entry(key).or_default() += count;
    }
    for (kind, count) in source.excluded_by_kind {
        *target.excluded_by_kind.entry(kind).or_default() += count;
    }
}

pub(crate) fn increment_symbol(counts: &mut BTreeMap<SymbolKey, usize>, key: SymbolKey) {
    *counts.entry(key).or_default() += 1;
}

pub(crate) fn increment_unique_symbol(counts: &mut BTreeMap<SymbolKey, usize>, key: SymbolKey) {
    counts.entry(key).or_insert(1);
}

pub(crate) fn symbol_count(counts: &BTreeMap<SymbolKey, usize>) -> usize {
    counts.values().sum()
}

pub(crate) fn count_difference(
    left: &BTreeMap<SymbolKey, usize>,
    right: &BTreeMap<SymbolKey, usize>,
) -> usize {
    left.iter()
        .map(|(key, count)| count.saturating_sub(*right.get(key).unwrap_or(&0)))
        .sum()
}

pub(crate) fn difference_examples(
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

pub(crate) fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        1.0
    } else {
        ((numerator as f64 / denominator as f64) * 10_000.0).round() / 10_000.0
    }
}
