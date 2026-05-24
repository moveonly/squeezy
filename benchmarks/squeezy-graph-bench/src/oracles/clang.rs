use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use serde_json::Value;
use squeezy_core::{LanguageKind, Result, SqueezyError};
use squeezy_graph::SemanticGraph;
use squeezy_workspace::{CrawlOptions, WorkspaceCrawler};

use crate::{
    accuracy::{compare_symbol_sets, increment_unique_symbol, merge_symbol_scan},
    cli::BenchmarkLanguage,
    mixed::select_scenarios,
    oracles::common_scan::{CFamilyClangSymbolScan, collect_c_family_squeezy_symbol_scan},
    report::{
        AccuracyReport, DefinitionAccuracyReport, NavigationAccuracyReport,
        ReferenceAccuracyReport, SymbolKey, SymbolScan,
    },
    util::{increment, truncate},
};

pub(crate) fn collect_c_family_accuracy(
    root: &Path,
    graph: &SemanticGraph,
    language: BenchmarkLanguage,
    oracle_file_limit: usize,
) -> Result<AccuracyReport> {
    let language_kind = language.source_language();
    let started = Instant::now();
    let oracle = collect_clang_ast_symbol_scan(root, language_kind, oracle_file_limit)?;
    let oracle_ms = started.elapsed().as_millis();
    let excluded_files = oracle
        .excluded_files
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let squeezy_symbols =
        collect_c_family_squeezy_symbol_scan(graph, language_kind, &excluded_files);
    let symbols = compare_symbol_sets(&squeezy_symbols, &oracle.symbols);
    let skipped = oracle.unparseable_files.len();
    let status = if skipped == 0 {
        format!(
            "clang AST JSON oracle succeeded for {}/{} selected {} files ({} candidates)",
            oracle.parsed_files,
            oracle.selected_files,
            language.as_str(),
            oracle.candidate_files
        )
    } else {
        format!(
            "clang AST JSON oracle parsed {}/{} selected {} files ({} candidates); skipped {} unparseable files excluded from Squeezy FP accounting: {}",
            oracle.parsed_files,
            oracle.selected_files,
            language.as_str(),
            oracle.candidate_files,
            skipped,
            oracle
                .unparseable_files
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    Ok(AccuracyReport {
        rust_analyzer_symbols_ms: Some(oracle_ms),
        rust_analyzer_symbol_status: status.clone(),
        symbols,
        navigation: NavigationAccuracyReport {
            rust_analyzer_lsp_ms: None,
            rust_analyzer_lsp_status: "clang AST navigation oracle not implemented".to_string(),
            requested_probe_limit: 0,
            definitions: DefinitionAccuracyReport::default(),
            references: ReferenceAccuracyReport::default(),
            limitations: vec![
                "C/C++ navigation FP/FN is not sampled against clangd yet; this benchmark currently compares declaration symbols against clang AST JSON.".to_string(),
            ],
        },
        limitations: vec![
            "C/C++ symbol TP/FP/FN compares file/name/kind declaration families exposed by Squeezy and clang AST JSON.".to_string(),
            "Headers and source files are parsed as standalone translation units with conservative include-dir heuristics; files requiring project-specific generated headers, compile flags, or compile_commands are reported as unparseable and excluded from Squeezy FP accounting.".to_string(),
            format!(
                "Oracle file limit is {}; use --oracle-files 0 for exhaustive local runs.",
                if oracle_file_limit == 0 {
                    "unlimited".to_string()
                } else {
                    oracle_file_limit.to_string()
                }
            ),
        ],
    })
}

pub(crate) fn collect_clang_ast_symbol_scan(
    root: &Path,
    language: LanguageKind,
    file_limit: usize,
) -> Result<CFamilyClangSymbolScan> {
    let snapshot = WorkspaceCrawler::new(CrawlOptions::default()).crawl(root)?;
    let root = fs::canonicalize(root)?;
    let mut records = snapshot
        .files
        .iter()
        .filter(|record| record.language == language)
        .cloned()
        .collect::<Vec<_>>();
    records.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let indexes = select_scenarios(records.len(), file_limit);
    let selected = indexes.iter().copied().collect::<BTreeSet<_>>();
    let include_dirs = clang_include_dirs(&root, &snapshot.files);
    let excluded_initial = records
        .iter()
        .enumerate()
        .filter(|(index, _)| !selected.contains(index))
        .map(|(_, record)| record.relative_path.clone())
        .collect::<Vec<_>>();

    let selected_records = indexes
        .iter()
        .copied()
        .map(|index| records[index].clone())
        .collect::<Vec<_>>();
    if selected_records.is_empty() {
        return Ok(CFamilyClangSymbolScan {
            symbols: SymbolScan::default(),
            parsed_files: 0,
            candidate_files: records.len(),
            selected_files: 0,
            unparseable_files: Vec::new(),
            excluded_files: excluded_initial,
        });
    }

    let worker_count = std::thread::available_parallelism()
        .map(|threads| threads.get())
        .unwrap_or(1)
        .min(selected_records.len())
        .max(1);
    let chunk_size = selected_records.len().div_ceil(worker_count);

    let outputs = std::thread::scope(|scope| -> Result<Vec<ClangAstFileOutput>> {
        let mut handles = Vec::new();
        for chunk in selected_records.chunks(chunk_size) {
            let chunk = chunk.to_vec();
            let include_dirs = include_dirs.clone();
            let root = root.clone();
            handles.push(scope.spawn(move || -> Result<Vec<ClangAstFileOutput>> {
                let mut out = Vec::with_capacity(chunk.len());
                for record in chunk {
                    let result =
                        clang_ast_symbols_for_file(&root, &record, language, &include_dirs);
                    out.push(ClangAstFileOutput {
                        relative_path: record.relative_path.clone(),
                        result,
                    });
                }
                Ok(out)
            }));
        }
        let mut outputs = Vec::with_capacity(selected_records.len());
        for handle in handles {
            match handle.join() {
                Ok(Ok(mut chunk_outputs)) => outputs.append(&mut chunk_outputs),
                Ok(Err(err)) => return Err(err),
                Err(_) => {
                    return Err(SqueezyError::Graph(
                        "clang AST oracle worker panicked".to_string(),
                    ));
                }
            }
        }
        Ok(outputs)
    })?;

    let mut scan = SymbolScan::default();
    let mut parsed_files = 0usize;
    let mut unparseable_files = Vec::new();
    let mut excluded_files = excluded_initial;
    for output in outputs {
        match output.result {
            Ok(file_scan) => {
                parsed_files += 1;
                merge_symbol_scan(&mut scan, file_scan);
            }
            Err(_) => {
                unparseable_files.push(output.relative_path.clone());
                excluded_files.push(output.relative_path);
            }
        }
    }

    Ok(CFamilyClangSymbolScan {
        symbols: scan,
        parsed_files,
        candidate_files: records.len(),
        selected_files: parsed_files + unparseable_files.len(),
        unparseable_files,
        excluded_files,
    })
}

pub(crate) struct ClangAstFileOutput {
    relative_path: String,
    result: Result<SymbolScan>,
}

pub(crate) fn clang_ast_symbols_for_file(
    root: &Path,
    record: &squeezy_workspace::FileRecord,
    language: LanguageKind,
    include_dirs: &[PathBuf],
) -> Result<SymbolScan> {
    let root = fs::canonicalize(root)?;
    let main_file = fs::canonicalize(&record.path)?;
    let compiler = match language {
        LanguageKind::C => "clang",
        LanguageKind::Cpp => "clang++",
        _ => {
            return Err(SqueezyError::Graph(format!(
                "clang AST oracle does not support {language:?}"
            )));
        }
    };
    let x_language = clang_x_language(record, language);
    let mut command = Command::new(compiler);
    command
        .current_dir(&root)
        .arg("-Xclang")
        .arg("-ast-dump=json")
        .arg("-fsyntax-only")
        .arg("-fno-color-diagnostics")
        .arg("-Wno-everything")
        .arg("-x")
        .arg(x_language);
    for include_dir in include_dirs {
        command.arg("-I").arg(include_dir);
    }
    command.arg(&main_file);

    let output = command.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default();
        return Err(SqueezyError::Graph(format!(
            "{compiler} AST failed for {} with {}{}",
            record.relative_path,
            output.status,
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {}", truncate(detail, 240))
            }
        )));
    }

    let ast: Value = serde_json::from_slice(&output.stdout)
        .map_err(|err| SqueezyError::Graph(format!("invalid clang AST JSON: {err}")))?;
    let mut scan = SymbolScan::default();
    collect_clang_ast_symbols_from_value(&ast, &root, &main_file, &record.relative_path, &mut scan);
    Ok(scan)
}

pub(crate) fn collect_clang_ast_symbols_from_value(
    node: &Value,
    root: &Path,
    main_file: &Path,
    relative_path: &str,
    scan: &mut SymbolScan,
) {
    if node.get("isImplicit").and_then(Value::as_bool) == Some(true) {
        return;
    }

    if let Some(kind) = clang_symbol_kind(node)
        && clang_node_is_in_main_file(node, root, main_file)
    {
        scan.raw_total += 1;
        if let Some(name) = node
            .get("name")
            .and_then(Value::as_str)
            .map(normalize_clang_symbol_name)
            .filter(|name| clang_symbol_name_is_comparable(name))
        {
            increment_unique_symbol(
                &mut scan.counts,
                SymbolKey {
                    file: relative_path.to_string(),
                    kind,
                    name,
                },
            );
        } else {
            increment(&mut scan.excluded_by_kind, "UnnamedOrOperator");
        }
    }

    if let Some(children) = node.get("inner").and_then(Value::as_array) {
        for child in children {
            collect_clang_ast_symbols_from_value(child, root, main_file, relative_path, scan);
        }
    }
}

pub(crate) fn clang_symbol_kind(node: &Value) -> Option<String> {
    let kind = node.get("kind").and_then(Value::as_str)?;
    match kind {
        "NamespaceDecl" => Some("Module".to_string()),
        "RecordDecl" => match node.get("tagUsed").and_then(Value::as_str) {
            Some("struct") => Some("Struct".to_string()),
            Some("union") => Some("Union".to_string()),
            _ => None,
        },
        "CXXRecordDecl" => match node.get("tagUsed").and_then(Value::as_str) {
            Some("struct") => Some("Struct".to_string()),
            Some("union") => Some("Union".to_string()),
            Some("class") | None => Some("Class".to_string()),
            _ => None,
        },
        "EnumDecl" => Some("Enum".to_string()),
        "FunctionDecl" => Some("Function".to_string()),
        "CXXMethodDecl" => Some("Method".to_string()),
        "TypedefDecl" | "TypeAliasDecl" => Some("TypeAlias".to_string()),
        _ => None,
    }
}

pub(crate) fn clang_node_is_in_main_file(node: &Value, root: &Path, main_file: &Path) -> bool {
    if node.pointer("/loc/includedFrom").is_some()
        || node.pointer("/range/begin/includedFrom").is_some()
    {
        return false;
    }
    let Some(raw_file) = clang_node_file(node) else {
        return true;
    };
    let path = PathBuf::from(raw_file);
    let absolute = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    fs::canonicalize(&absolute)
        .map(|path| path == main_file)
        .unwrap_or(false)
}

pub(crate) fn clang_node_file(node: &Value) -> Option<&str> {
    node.pointer("/loc/file")
        .and_then(Value::as_str)
        .or_else(|| node.pointer("/range/begin/file").and_then(Value::as_str))
}

pub(crate) fn clang_x_language(
    record: &squeezy_workspace::FileRecord,
    language: LanguageKind,
) -> &'static str {
    let extension = record
        .path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    match (language, extension) {
        (LanguageKind::C, "h") => "c-header",
        (LanguageKind::Cpp, "h" | "hh" | "hpp" | "hxx") => "c++-header",
        (LanguageKind::C, _) => "c",
        (LanguageKind::Cpp, _) => "c++",
        _ => "c",
    }
}

pub(crate) fn clang_include_dirs(
    root: &Path,
    files: &[squeezy_workspace::FileRecord],
) -> Vec<PathBuf> {
    let mut dirs = BTreeSet::new();
    dirs.insert(root.to_path_buf());
    for name in ["include", "src", "lib", "deps"] {
        let path = root.join(name);
        if path.is_dir() {
            dirs.insert(path);
        }
    }
    for file in files
        .iter()
        .filter(|file| matches!(file.language, LanguageKind::C | LanguageKind::Cpp))
    {
        let Some(parent) = file.path.parent() else {
            continue;
        };
        let parent_name = parent
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if matches!(parent_name, "include" | "inc" | "src" | "lib" | "deps") {
            dirs.insert(parent.to_path_buf());
        }
    }
    let mut dirs = dirs.into_iter().collect::<Vec<_>>();
    dirs.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then(left.cmp(right))
    });
    dirs.truncate(128);
    dirs
}

pub(crate) fn normalize_clang_symbol_name(name: &str) -> String {
    name.trim()
        .trim_start_matches('~')
        .split('<')
        .next()
        .unwrap_or(name)
        .rsplit("::")
        .next()
        .unwrap_or(name)
        .to_string()
}

pub(crate) fn clang_symbol_name_is_comparable(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with("__")
        && !name.starts_with("operator")
        && !name.contains("(anonymous")
}

#[cfg(test)]
#[path = "clang_tests.rs"]
mod tests;
