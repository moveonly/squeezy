pub(crate) fn collect_accuracy(root: &Path, graph: &SemanticGraph, ra_lsp_probes: usize) -> AccuracyReport {
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

include!("oracles/clang.rs");
include!("oracles/cpython_ast.rs");
include!("oracles/tsc.rs");
include!("oracles/go_types.rs");
include!("oracles/common_scan.rs");
include!("oracles/roslyn.rs");
include!("oracles/javac.rs");
include!("oracles/rust_analyzer.rs");
pub(crate) fn compare_symbol_sets(squeezy: &SymbolScan, rust_analyzer: &SymbolScan) -> AccuracySetReport {
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

pub(crate) fn probe_byte_for_edge(source: &str, start: usize, end: usize, target_text: &str) -> usize {
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

#[derive(Debug, Clone)]
pub(crate) enum MixedScenario {
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
    pub(crate) fn tool(&self) -> &'static str {
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

pub(crate) fn build_mixed_scenarios(graph: &SemanticGraph) -> Vec<MixedScenario> {
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

pub(crate) fn select_scenarios(available: usize, requested: usize) -> Vec<usize> {
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

pub(crate) fn run_mixed_scenario(graph: &SemanticGraph, scenario: &MixedScenario) -> usize {
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

pub(crate) fn run_refresh_probe(repo: &Path, language: BenchmarkLanguage) -> Result<RefreshProbeReport> {
    let source_snapshot = WorkspaceCrawler::new(CrawlOptions::default()).crawl(repo)?;
    let temp_root = temp_dir("squeezy-refresh-probe")?;
    let mut copied = Vec::new();
    for record in source_snapshot
        .files
        .iter()
        .filter(|record| record.language == language.language_kind())
        .take(250)
    {
        let dest = temp_root.join(&record.relative_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&record.path, &dest)?;
        copied.push(dest);
    }

    // The probe creates a synthetic tree of just-source files, so the
    // workspace indexing-signal check can fail (no Cargo.toml/pom.xml/etc.
    // gets copied alongside the source files). Disable the signal
    // requirement for the probe so refresh always walks the temp tree.
    let crawl_options = CrawlOptions {
        require_indexing_signal: false,
        ..CrawlOptions::default()
    };
    let mut manager = GraphManager::open_with_crawl_options(
        &temp_root,
        RefreshConfig {
            debounce: std::time::Duration::from_millis(0),
            idle_refresh_interval: std::time::Duration::from_millis(0),
            per_tool_refresh_budget: std::time::Duration::from_secs(10),
        },
        crawl_options,
    )?;

    let edits = copied.iter().take(2).cloned().collect::<Vec<_>>();
    for path in &edits {
        let mut text = fs::read_to_string(path)?;
        text.push_str(language.comment_text());
        fs::write(path, text)?;
        manager.record_changed_path(path.clone());
    }

    let refresh_started = Instant::now();
    let report = manager.refresh_before_query()?;
    let refresh_ms = refresh_started.elapsed().as_millis();
    fs::remove_dir_all(&temp_root)?;

    Ok(RefreshProbeReport {
        language: language.as_str().to_string(),
        copied_source_files: copied.len(),
        edited_files: edits.len(),
        refresh_ms,
        reparsed_files: report.reparsed_files,
        changed_files: report.changed_files.len(),
        changed_paths_from_events: report.changed_paths_from_events,
        changed_paths_from_polling: report.changed_paths_from_polling,
        unchanged_event_paths: report.unchanged_event_paths,
        budget_exhausted: report.budget_exhausted,
    })
}

