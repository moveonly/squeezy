use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    time::Instant,
};

use squeezy_core::{EdgeKind, LanguageKind, Result, SymbolId, SymbolKind};
use squeezy_graph::{BodySearchQuery, GraphManager, RefreshConfig, SemanticGraph, SignatureQuery};
use squeezy_parse::BodyHitKind;
use squeezy_workspace::{CrawlOptions, WorkspaceCrawler};

use crate::{
    accuracy::{collect_accuracy, empty_accuracy},
    cli::BenchmarkLanguage,
    execution::build_graph,
    harness::toolchain::{
        time_cargo_check_optional, time_clang_syntax_optional, time_dotnet_build_optional,
    },
    oracles::{
        collect_c_family_accuracy, collect_csharp_oracle_accuracy, collect_go_oracle_accuracy,
        collect_js_ts_accuracy, csharp_oracle_to_accuracy, go_oracle_to_accuracy,
        time_js_ts_oracle, time_php_oracle_optional, time_rust_analyzer,
    },
    report::{MixedWorkloadReport, RefreshProbeReport},
    util::{DeterministicRng, increment, temp_dir},
};

pub(crate) fn run_mixed_workload(
    repo: &Path,
    language: BenchmarkLanguage,
    requested_scenarios: usize,
    ra_lsp_probes: usize,
    oracle_files: usize,
) -> Result<MixedWorkloadReport> {
    let (compiler_check_ms, compiler_check_status) = match language {
        BenchmarkLanguage::C => time_clang_syntax_optional(repo, "clang", LanguageKind::C),
        BenchmarkLanguage::CSharp => time_dotnet_build_optional(repo),
        BenchmarkLanguage::Cpp => time_clang_syntax_optional(repo, "clang++", LanguageKind::Cpp),
        BenchmarkLanguage::Go => (
            None,
            "mixed workload compiler check not used for Go".to_string(),
        ),
        BenchmarkLanguage::Java => (
            None,
            "mixed workload compiler check not used for Java".to_string(),
        ),
        BenchmarkLanguage::Kotlin => (
            None,
            "mixed workload unsupported for Kotlin".to_string(),
        ),
        BenchmarkLanguage::Rust => time_cargo_check_optional(repo),
        BenchmarkLanguage::Php => {
            let (ms, status) = time_php_oracle_optional(repo);
            (if ms == 0 { None } else { Some(ms) }, status)
        }
        BenchmarkLanguage::Python => (None, "mixed workload unsupported for Python".to_string()),
        BenchmarkLanguage::Ruby => (None, "mixed workload unsupported for Ruby".to_string()),
        BenchmarkLanguage::Swift => (None, "mixed workload unsupported for Swift".to_string()),
        BenchmarkLanguage::JavaScript | BenchmarkLanguage::TypeScript => {
            match time_js_ts_oracle(repo) {
                Ok(ms) => (Some(ms), "TypeScript compiler API oracle".to_string()),
                Err(err) => (
                    None,
                    format!("TypeScript compiler API oracle unavailable: {err}"),
                ),
            }
        }
    };
    let (rust_analyzer_ms, rust_analyzer_status) = if language == BenchmarkLanguage::Rust {
        time_rust_analyzer(repo)
    } else {
        (
            None,
            "rust-analyzer oracle not used for this language".to_string(),
        )
    };

    let build = build_graph(repo)?;
    let graph = build.graph;
    let squeezy_build_ms = build.phases.total_ms;
    let accuracy = match language {
        BenchmarkLanguage::Rust => collect_accuracy(repo, &graph, ra_lsp_probes),
        BenchmarkLanguage::CSharp => match collect_csharp_oracle_accuracy(repo, &graph) {
            Ok(report) => csharp_oracle_to_accuracy(&report),
            Err(err) => empty_accuracy(&format!("C# semantic oracle failed: {err}")),
        },
        BenchmarkLanguage::C | BenchmarkLanguage::Cpp => {
            collect_c_family_accuracy(repo, &graph, language, oracle_files)?
        }
        BenchmarkLanguage::Go => match collect_go_oracle_accuracy(repo, &graph) {
            Ok(report) => go_oracle_to_accuracy(&report),
            Err(err) => empty_accuracy(&format!("Go semantic oracle failed: {err}")),
        },
        BenchmarkLanguage::Java => {
            empty_accuracy("mixed workload accuracy oracle not used for Java")
        }
        BenchmarkLanguage::Kotlin => empty_accuracy("mixed workload unsupported for Kotlin"),
        BenchmarkLanguage::Php => {
            empty_accuracy("mixed workload accuracy oracle not used for PHP")
        }
        BenchmarkLanguage::Python => empty_accuracy("mixed workload unsupported for Python"),
        BenchmarkLanguage::Ruby => empty_accuracy("mixed workload unsupported for Ruby"),
        BenchmarkLanguage::Swift => empty_accuracy("mixed workload unsupported for Swift"),
        BenchmarkLanguage::JavaScript | BenchmarkLanguage::TypeScript => {
            collect_js_ts_accuracy(repo, &graph, ra_lsp_probes)
        }
    };

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
    let refresh_probe = run_refresh_probe(repo, language)?;

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

pub(crate) fn run_refresh_probe(
    repo: &Path,
    language: BenchmarkLanguage,
) -> Result<RefreshProbeReport> {
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

#[cfg(test)]
#[path = "mixed_tests.rs"]
mod tests;
