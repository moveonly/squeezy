use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use squeezy_core::{
    EdgeKind, LanguageKind, Result, SqueezyError, SymbolId, SymbolKind,
};
use squeezy_graph::{BodySearchQuery, GraphManager, RefreshConfig, SemanticGraph, SignatureQuery};
use squeezy_parse::{BodyHitKind, LanguageParser, ParsedFile};
use squeezy_workspace::{CrawlOptions, WorkspaceCrawler};

use crate::report::*;
use crate::cli::{Args, BenchmarkLanguage};
use crate::summary::{print_summary, write_report};

pub fn main() -> Result<()> {
    let args = Args::parse()?;
    let spec_text = fs::read_to_string(&args.spec)?;
    let spec: QuerySpecFile = serde_json::from_str(&spec_text)
        .map_err(|err| SqueezyError::Graph(format!("invalid benchmark spec: {err}")))?;

    let (validation_ms, validation_status) = match args.language {
        BenchmarkLanguage::C => (
            time_clang_syntax(&args.fixture, "clang", LanguageKind::C)?,
            "clang -fsyntax-only".to_string(),
        ),
        BenchmarkLanguage::Cpp => (
            time_clang_syntax(&args.fixture, "clang++", LanguageKind::Cpp)?,
            "clang++ -fsyntax-only".to_string(),
        ),
        BenchmarkLanguage::CSharp => (
            time_dotnet_build(&args.fixture)?,
            "dotnet build".to_string(),
        ),
        BenchmarkLanguage::Java => time_java_oracle_optional(&args.fixture),
        BenchmarkLanguage::Rust => (time_cargo_check(&args.fixture)?, "cargo check".to_string()),
        BenchmarkLanguage::Python => (
            time_python_ast_oracle(&args.fixture)?,
            "CPython ast oracle".to_string(),
        ),
        BenchmarkLanguage::Go => (
            time_go_ast_oracle(&args.fixture)?,
            "Go parser/type oracle".to_string(),
        ),
        BenchmarkLanguage::JavaScript | BenchmarkLanguage::TypeScript => {
            match time_js_ts_oracle(&args.fixture) {
                Ok(ms) => (ms, "TypeScript compiler API oracle".to_string()),
                Err(err) => (
                    0,
                    format!("TypeScript compiler API oracle unavailable: {err}"),
                ),
            }
        }
    };

    let build = build_graph(&args.fixture)?;
    let graph = build.graph;
    let squeezy_build_ms = build.phases.total_ms;

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
        BenchmarkLanguage::CSharp => empty_accuracy("rust-analyzer oracle not used for C#"),
        BenchmarkLanguage::C | BenchmarkLanguage::Cpp => {
            collect_c_family_accuracy(&args.fixture, &graph, args.language, args.oracle_files)?
        }
        BenchmarkLanguage::Python => empty_accuracy("rust-analyzer oracle not used for Python"),
        BenchmarkLanguage::Go => empty_accuracy("rust-analyzer oracle not used for Go"),
        BenchmarkLanguage::JavaScript | BenchmarkLanguage::TypeScript => {
            collect_js_ts_accuracy(&args.fixture, &graph, args.ra_lsp_probes)
        }
    };
    let python_oracle = match args.language {
        BenchmarkLanguage::Python => Some(collect_python_oracle_accuracy(&args.fixture, &graph)?),
        _ => None,
    };
    let go_oracle = match args.language {
        BenchmarkLanguage::Go => Some(collect_go_oracle_accuracy(&args.fixture, &graph)?),
        _ => None,
    };
    let js_ts_oracle = match args.language {
        BenchmarkLanguage::JavaScript | BenchmarkLanguage::TypeScript => {
            Some(collect_js_ts_oracle_accuracy(&args.fixture, &graph))
        }
        _ => None,
    };
    let csharp_oracle = match args.language {
        BenchmarkLanguage::CSharp => Some(collect_csharp_oracle_accuracy(&args.fixture, &graph)?),
        _ => None,
    };
    let java_oracle = match args.language {
        BenchmarkLanguage::Java => Some(collect_java_oracle_accuracy(
            &args.fixture,
            &graph,
            &query_reports,
        )?),
        _ => None,
    };
    let faster_than_validation =
        validation_status.starts_with("skipped") || squeezy_total_ms < validation_ms;

    let mixed_workload = if args.language.supports_mixed_workload() {
        args.mixed_repo
            .as_ref()
            .map(|repo| {
                run_mixed_workload(
                    repo,
                    args.language,
                    args.mixed_iterations,
                    args.ra_lsp_probes,
                    args.oracle_files,
                )
            })
            .transpose()?
    } else {
        None
    };

    let stats = graph.stats();
    let refresh_probe = Some(run_refresh_probe(&args.fixture, args.language)?);
    let heuristic_iterations = heuristic_iteration_reports(args.language, &go_oracle);
    let report = BenchmarkReport {
        language: args.language.as_str().to_string(),
        fixture: args.fixture.display().to_string(),
        spec: args.spec.display().to_string(),
        validation_ms,
        validation_status,
        squeezy_build_ms,
        squeezy_query_ms,
        squeezy_total_ms,
        build_phases: build.phases,
        faster_than_validation: validation_ms == 0 || faster_than_validation,
        graph: GraphReport {
            files: stats.files,
            symbols: stats.symbols,
            edges: stats.edges,
            body_hits: stats.body_hits,
            references: stats.references,
            calls: stats.calls,
            body_hit_trigram_indexed: stats.body_hit_trigram_indexed,
            body_hit_trigram_terms: stats.body_hit_trigram_terms,
            reference_index_terms: stats.reference_index_terms,
        },
        accuracy,
        python_oracle,
        js_ts_oracle,
        java_oracle,
        csharp_oracle,
        go_oracle,
        refresh_probe,
        heuristic_iterations,
        queries: query_reports,
        mixed_workload,
    };

    write_report(&args.report, &report)?;
    print_summary(&report);
    enforce_gates(&report, args.no_speed_gate)
}

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

include!("mixed.rs");
include!("accuracy.rs");
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

pub(crate) fn time_cargo_check(fixture: &Path) -> Result<u128> {
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

pub(crate) fn time_dotnet_build(fixture: &Path) -> Result<u128> {
    let build_target = find_dotnet_build_target(fixture);
    let started = Instant::now();
    let mut command = Command::new("dotnet");
    command.arg("build");
    if let Some(target) = build_target {
        command.arg(target);
    }
    let status = command
        .arg("--nologo")
        .arg("-v")
        .arg("minimal")
        .current_dir(fixture)
        .status()?;
    let elapsed = started.elapsed().as_millis();
    if status.success() {
        Ok(elapsed)
    } else {
        Err(SqueezyError::Graph(format!(
            "dotnet build validation failed with {status}"
        )))
    }
}

pub(crate) fn find_dotnet_build_target(root: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    collect_dotnet_build_targets(root, root, 0, &mut candidates);
    // Prefer the shallowest candidate (root-level solution beats nested project),
    // then by extension priority (slnx > sln > csproj), then lexicographic.
    candidates.sort_by(|left, right| {
        left.1
            .cmp(&right.1)
            .then_with(|| left.0.cmp(&right.0))
            .then_with(|| left.2.cmp(&right.2))
    });
    candidates.into_iter().map(|(_, _, path)| path).next()
}

pub(crate) fn collect_dotnet_build_targets(
    root: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<(usize, usize, PathBuf)>,
) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries = entries.filter_map(|entry| entry.ok()).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if matches!(name, ".git" | "bin" | "obj" | "packages" | "target") {
                continue;
            }
            collect_dotnet_build_targets(root, &path, depth + 1, out);
            continue;
        }
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            continue;
        };
        let priority = match extension {
            "slnx" => 0,
            "sln" => 1,
            "csproj" => 2,
            _ => continue,
        };
        let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        out.push((priority, depth, relative));
    }
}

pub(crate) fn time_python_ast_oracle(fixture: &Path) -> Result<u128> {
    let started = Instant::now();
    let _ = collect_python_ast_symbol_scan(fixture)?;
    Ok(started.elapsed().as_millis())
}

pub(crate) fn time_dotnet_build_optional(repo: &Path) -> (Option<u128>, String) {
    match time_dotnet_build(repo) {
        Ok(ms) => (Some(ms), "dotnet build succeeded".to_string()),
        Err(err) => (None, format!("dotnet build failed: {err}")),
    }
}

pub(crate) fn time_clang_syntax(fixture: &Path, compiler: &str, language: LanguageKind) -> Result<u128> {
    let snapshot = WorkspaceCrawler::new(CrawlOptions::default()).crawl(fixture)?;
    let files = snapshot
        .files
        .into_iter()
        .filter(|record| record.language == language)
        .filter(|record| {
            !matches!(
                record
                    .path
                    .extension()
                    .and_then(|extension| extension.to_str()),
                Some("h" | "hh" | "hpp" | "hxx")
            )
        })
        .collect::<Vec<_>>();
    if files.is_empty() {
        return Ok(0);
    }

    let started = Instant::now();
    let worker_count = std::thread::available_parallelism()
        .map(|threads| threads.get())
        .unwrap_or(1)
        .min(files.len())
        .max(1);
    let chunk_size = files.len().div_ceil(worker_count);
    let compiler = compiler.to_string();
    std::thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::new();
        for chunk in files.chunks(chunk_size) {
            let chunk = chunk.to_vec();
            let compiler = compiler.clone();
            handles.push(scope.spawn(move || -> Result<()> {
                for file in chunk {
                    let status = Command::new(&compiler)
                        .arg("-fsyntax-only")
                        .arg(&file.path)
                        .status()?;
                    if !status.success() {
                        return Err(SqueezyError::Graph(format!(
                            "{compiler} validation failed for {} with {status}",
                            file.relative_path
                        )));
                    }
                }
                Ok(())
            }));
        }
        for handle in handles {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(err)) => return Err(err),
                Err(_) => {
                    return Err(SqueezyError::Graph(
                        "clang syntax worker panicked".to_string(),
                    ));
                }
            }
        }
        Ok(())
    })?;
    Ok(started.elapsed().as_millis())
}

pub(crate) fn time_clang_syntax_optional(
    repo: &Path,
    compiler: &str,
    language: LanguageKind,
) -> (Option<u128>, String) {
    match time_clang_syntax(repo, compiler, language) {
        Ok(ms) => (Some(ms), format!("{compiler} -fsyntax-only succeeded")),
        Err(err) => (None, format!("{compiler} -fsyntax-only failed: {err}")),
    }
}

pub(crate) fn time_go_ast_oracle(fixture: &Path) -> Result<u128> {
    let started = Instant::now();
    let _ = collect_go_ast_symbol_scan(fixture)?;
    Ok(started.elapsed().as_millis())
}

pub(crate) fn time_cargo_check_optional(repo: &Path) -> (Option<u128>, String) {
    match time_cargo_check(repo) {
        Ok(ms) => (Some(ms), "cargo check succeeded".to_string()),
        Err(err) => (None, format!("cargo check failed: {err}")),
    }
}

pub(crate) fn time_rust_analyzer(repo: &Path) -> (Option<u128>, String) {
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

pub(crate) fn parse_lsp_locations(value: &Value, root: &Path) -> Result<Vec<LocationKey>> {
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

pub(crate) fn parse_lsp_location(value: &Value, root: &Path) -> Result<LocationKey> {
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

pub(crate) fn location_file_key(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .map(|relative| relative.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

pub(crate) fn path_to_file_uri(path: &Path) -> Result<String> {
    let path = fs::canonicalize(path)?;
    let raw = path.to_string_lossy();
    Ok(format!("file://{}", percent_encode_path(&raw)))
}

pub(crate) fn file_uri_to_path(uri: &str) -> Result<PathBuf> {
    let raw = uri
        .strip_prefix("file://")
        .ok_or_else(|| SqueezyError::Graph(format!("unsupported non-file URI {uri}")))?;
    Ok(PathBuf::from(percent_decode(raw)?))
}

pub(crate) fn percent_encode_path(path: &str) -> String {
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

pub(crate) fn percent_decode(value: &str) -> Result<String> {
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

pub(crate) fn byte_to_lsp_position(source: &str, byte: usize) -> LspPosition {
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

pub(crate) fn line_char_to_byte(source: &str, line: u32, character: u32) -> Option<usize> {
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

pub(crate) fn rust_analyzer_command() -> Option<Command> {
    rust_analyzer_program().map(Command::new)
}

pub(crate) fn rust_analyzer_program() -> Option<String> {
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

pub(crate) fn command_exists(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub(crate) fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect::<String>()
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

    if let Some(refresh) = &report.refresh_probe
        && refresh.reparsed_files != refresh.edited_files
    {
        return Err(SqueezyError::Graph(format!(
            "refresh probe reparsed {} files after {} edits",
            refresh.reparsed_files, refresh.edited_files
        )));
    }

    if !no_speed_gate
        && let Some(go) = &report.go_oracle
        && (go.symbols.false_positive != 0 || go.symbols.false_negative != 0)
    {
        return Err(SqueezyError::Graph(format!(
            "Go oracle accuracy regressed: fp={} fn={}",
            go.symbols.false_positive, go.symbols.false_negative
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

pub(crate) fn increment(counts: &mut BTreeMap<String, usize>, key: &str) {
    *counts.entry(key.to_string()).or_default() += 1;
}

pub(crate) fn temp_dir(prefix: &str) -> Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| SqueezyError::Graph(format!("clock error: {err}")))?
        .as_nanos();
    let path = env::temp_dir().join(format!("{prefix}-{nonce}"));
    fs::create_dir_all(&path)?;
    Ok(path)
}

pub(crate) fn required<'a>(value: &'a Option<String>, name: &str) -> Result<&'a str> {
    value
        .as_deref()
        .ok_or_else(|| SqueezyError::Graph(format!("query missing required {name}")))
}

pub(crate) struct DeterministicRng {
    pub(crate) state: u64,
}

impl DeterministicRng {
    pub(crate) fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub(crate) fn next_usize(&mut self, upper_bound: usize) -> usize {
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


#[cfg(test)]
#[path = "runner_tests.rs"]
mod tests;
