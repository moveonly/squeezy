use std::{
    collections::{HashMap, HashSet},
    fs,
};

use squeezy_core::{
    Confidence, ContentHash, EdgeKind, FileId, Freshness, LanguageKind, Provenance, Result,
    SourcePoint, SourceSpan, SqueezyError, SymbolId, SymbolKind,
};
use squeezy_workspace::FileRecord;
use tree_sitter::{InputEdit, Node, Parser, Point, Tree};

pub const CRATE_NAME: &str = "squeezy-parse";
const PARALLEL_PARSE_THRESHOLD: usize = 8;

pub fn crate_name() -> &'static str {
    CRATE_NAME
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFile {
    pub file: FileRecord,
    pub package: Option<String>,
    pub symbols: Vec<ParsedSymbol>,
    pub imports: Vec<ParsedImport>,
    pub calls: Vec<ParsedCall>,
    pub references: Vec<ParsedReference>,
    pub body_hits: Vec<BodyHit>,
    pub unsupported: Option<UnsupportedParse>,
    pub diagnostics: Vec<ParseDiagnostic>,
    pub changed_ranges: Vec<SourceSpan>,
}

impl ParsedFile {
    pub fn unsupported(file: FileRecord, reason: impl Into<String>) -> Self {
        Self {
            unsupported: Some(UnsupportedParse {
                reason: reason.into(),
                suggested_fallback: "bounded read/grep/list navigation".to_string(),
            }),
            file,
            package: None,
            symbols: Vec::new(),
            imports: Vec::new(),
            calls: Vec::new(),
            references: Vec::new(),
            body_hits: Vec::new(),
            diagnostics: Vec::new(),
            changed_ranges: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSymbol {
    pub id: SymbolId,
    pub file_id: FileId,
    pub parent_id: Option<SymbolId>,
    pub name: String,
    pub kind: SymbolKind,
    pub span: SourceSpan,
    pub body_span: Option<SourceSpan>,
    pub signature: String,
    pub visibility: Option<String>,
    pub docs: Vec<String>,
    pub attributes: Vec<String>,
    pub provenance: Provenance,
    pub confidence: Confidence,
    pub freshness: Freshness,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedImport {
    pub file_id: FileId,
    pub owner_id: Option<SymbolId>,
    pub path: String,
    pub alias: Option<String>,
    pub is_glob: bool,
    pub is_reexport: bool,
    pub span: SourceSpan,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCall {
    pub file_id: FileId,
    pub caller_id: Option<SymbolId>,
    pub name: String,
    pub target_text: String,
    pub receiver: Option<String>,
    pub arity: usize,
    pub kind: ParsedCallKind,
    pub span: SourceSpan,
    pub provenance: Provenance,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedCallKind {
    Direct,
    Method,
    Macro,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedReference {
    pub file_id: FileId,
    pub owner_id: Option<SymbolId>,
    pub text: String,
    pub kind: ReferenceKind,
    pub span: SourceSpan,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReferenceKind {
    Identifier,
    Type,
    Path,
    Field,
    Attribute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyHit {
    pub file_id: FileId,
    pub owner_id: Option<SymbolId>,
    pub text: String,
    pub kind: BodyHitKind,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyHitKind {
    Identifier,
    Type,
    Path,
    Call,
    Macro,
    Literal,
    Attribute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedParse {
    pub reason: String,
    pub suggested_fallback: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDiagnostic {
    pub message: String,
    pub span: Option<SourceSpan>,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParseSummary {
    pub parsed_files: usize,
    pub unsupported_files: usize,
    pub changed_files: usize,
    pub changed_ranges: usize,
}

#[derive(Debug, Clone)]
struct CachedParsedFile {
    hash: ContentHash,
    language: LanguageKind,
    source: String,
    tree: Tree,
}

pub struct LanguageParser {
    go_parser: Parser,
    rust_parser: Parser,
    python_parser: Parser,
    c_parser: Parser,
    cpp_parser: Parser,
    cache: HashMap<FileId, CachedParsedFile>,
}

/// Back-compat alias kept while existing call sites migrate from the original
/// Rust-only name. New code should prefer [`LanguageParser`].
pub type RustParser = LanguageParser;

#[derive(Debug, Clone)]
struct ParseJob {
    index: usize,
    record: FileRecord,
    old: Option<CachedParsedFile>,
}

struct ParseOutput {
    index: usize,
    parsed: ParsedFile,
    cache: Option<CachedParsedFile>,
}

impl LanguageParser {
    pub fn new() -> Result<Self> {
        let go_parser = parser_with_go_language()?;
        let rust_parser = parser_with_rust_language()?;
        let python_parser = parser_with_python_language()?;
        let c_parser = parser_with_c_language()?;
        let cpp_parser = parser_with_cpp_language()?;
        Ok(Self {
            go_parser,
            rust_parser,
            python_parser,
            c_parser,
            cpp_parser,
            cache: HashMap::new(),
        })
    }

    pub fn parse_records(
        &mut self,
        records: &[FileRecord],
    ) -> Result<(Vec<ParsedFile>, ParseSummary)> {
        if records.len() >= PARALLEL_PARSE_THRESHOLD {
            return self.parse_records_parallel(records);
        }
        self.parse_records_serial(records)
    }

    fn parse_records_serial(
        &mut self,
        records: &[FileRecord],
    ) -> Result<(Vec<ParsedFile>, ParseSummary)> {
        let mut parsed = Vec::with_capacity(records.len());
        let mut summary = ParseSummary::default();

        for record in records {
            let parsed_file = self.parse_record(record)?;
            update_parse_summary(&mut summary, &parsed_file);
            parsed.push(parsed_file);
        }

        Ok((parsed, summary))
    }

    fn parse_records_parallel(
        &mut self,
        records: &[FileRecord],
    ) -> Result<(Vec<ParsedFile>, ParseSummary)> {
        let worker_count = std::thread::available_parallelism()
            .map(|threads| threads.get())
            .unwrap_or(1)
            .min(records.len());
        if worker_count <= 1 {
            return self.parse_records_serial(records);
        }

        let jobs = records
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, record)| {
                let old = self.cache.remove(&record.id);
                ParseJob { index, record, old }
            })
            .collect::<Vec<_>>();
        let chunk_size = jobs.len().div_ceil(worker_count);
        let mut outputs = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for chunk in jobs.chunks(chunk_size) {
                let chunk = chunk.to_vec();
                handles.push(scope.spawn(move || parse_job_chunk(chunk)));
            }

            let mut outputs = Vec::with_capacity(records.len());
            for handle in handles {
                match handle.join() {
                    Ok(Ok(mut chunk_outputs)) => outputs.append(&mut chunk_outputs),
                    Ok(Err(err)) => return Err(err),
                    Err(_) => {
                        return Err(SqueezyError::Parse(
                            "parallel parse worker panicked".to_string(),
                        ));
                    }
                }
            }
            Ok(outputs)
        })?;

        outputs.sort_by_key(|output| output.index);
        let mut parsed = Vec::with_capacity(outputs.len());
        let mut summary = ParseSummary::default();
        for output in outputs {
            if let Some(cache) = output.cache {
                self.cache.insert(output.parsed.file.id.clone(), cache);
            }
            update_parse_summary(&mut summary, &output.parsed);
            parsed.push(output.parsed);
        }

        Ok((parsed, summary))
    }

    pub fn parse_record(&mut self, record: &FileRecord) -> Result<ParsedFile> {
        if !is_supported_language(record.language) {
            self.cache.remove(&record.id);
            return Ok(ParsedFile::unsupported(
                record.clone(),
                format!("unsupported language for {}", record.relative_path),
            ));
        }

        let source = match fs::read_to_string(&record.path) {
            Ok(source) => source,
            Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
                self.cache.remove(&record.id);
                return Ok(ParsedFile::unsupported(
                    record.clone(),
                    format!("non-UTF-8 source for {}", record.relative_path),
                ));
            }
            Err(err) => return Err(err.into()),
        };
        self.parse_source(record, source)
    }

    pub fn parse_source(&mut self, record: &FileRecord, source: String) -> Result<ParsedFile> {
        if !is_supported_language(record.language) {
            self.cache.remove(&record.id);
            return Ok(ParsedFile::unsupported(
                record.clone(),
                format!("unsupported language for {}", record.relative_path),
            ));
        }

        let old = self.cache.remove(&record.id);
        let (tree, changed_ranges) = match old.filter(|cached| cached.language == record.language) {
            Some(mut cached) if cached.hash != record.hash => {
                let edit = input_edit(&cached.source, &source);
                cached.tree.edit(&edit);
                let parser = self.parser_for_language(record.language)?;
                let new_tree = parser.parse(&source, Some(&cached.tree)).ok_or_else(|| {
                    SqueezyError::Parse(format!(
                        "tree-sitter returned no {:?} tree",
                        record.language
                    ))
                })?;
                let mut changed_ranges = cached
                    .tree
                    .changed_ranges(&new_tree)
                    .map(span_from_range)
                    .collect::<Vec<_>>();
                if changed_ranges.is_empty() {
                    changed_ranges.push(span_from_edit(&edit));
                }
                (new_tree, changed_ranges)
            }
            Some(cached) => {
                self.cache.insert(record.id.clone(), cached.clone());
                let mut parsed = extract_language(record.clone(), &source, &cached.tree);
                parsed.changed_ranges = Vec::new();
                return Ok(parsed);
            }
            None => {
                let parser = self.parser_for_language(record.language)?;
                let tree = parser.parse(&source, None).ok_or_else(|| {
                    SqueezyError::Parse(format!(
                        "tree-sitter returned no {:?} tree",
                        record.language
                    ))
                })?;
                (tree, Vec::new())
            }
        };

        let mut parsed = extract_language(record.clone(), &source, &tree);
        parsed.changed_ranges = changed_ranges;
        self.cache.insert(
            record.id.clone(),
            CachedParsedFile {
                hash: record.hash.clone(),
                language: record.language,
                source,
                tree,
            },
        );
        Ok(parsed)
    }

    fn parser_for_language(&mut self, language: LanguageKind) -> Result<&mut Parser> {
        match language {
            LanguageKind::C => Ok(&mut self.c_parser),
            LanguageKind::Cpp => Ok(&mut self.cpp_parser),
            LanguageKind::Go => Ok(&mut self.go_parser),
            LanguageKind::Rust => Ok(&mut self.rust_parser),
            LanguageKind::Python => Ok(&mut self.python_parser),
            _ => Err(SqueezyError::Parse(format!(
                "unsupported parser language {language:?}"
            ))),
        }
    }
}

fn parse_job_chunk(jobs: Vec<ParseJob>) -> Result<Vec<ParseOutput>> {
    let mut parsers = WorkerParsers {
        go: parser_with_go_language()?,
        rust: parser_with_rust_language()?,
        python: parser_with_python_language()?,
        c: parser_with_c_language()?,
        cpp: parser_with_cpp_language()?,
    };
    let mut outputs = Vec::with_capacity(jobs.len());
    for job in jobs {
        outputs.push(parse_record_with_cache(&mut parsers, job)?);
    }
    Ok(outputs)
}

struct WorkerParsers {
    go: Parser,
    rust: Parser,
    python: Parser,
    c: Parser,
    cpp: Parser,
}

impl WorkerParsers {
    fn parser_for_language(&mut self, language: LanguageKind) -> Result<&mut Parser> {
        match language {
            LanguageKind::C => Ok(&mut self.c),
            LanguageKind::Cpp => Ok(&mut self.cpp),
            LanguageKind::Go => Ok(&mut self.go),
            LanguageKind::Rust => Ok(&mut self.rust),
            LanguageKind::Python => Ok(&mut self.python),
            _ => Err(SqueezyError::Parse(format!(
                "unsupported parser language {language:?}"
            ))),
        }
    }
}

fn parse_record_with_cache(parsers: &mut WorkerParsers, job: ParseJob) -> Result<ParseOutput> {
    let ParseJob { index, record, old } = job;
    if !is_supported_language(record.language) {
        return Ok(ParseOutput {
            index,
            parsed: ParsedFile::unsupported(
                record.clone(),
                format!("unsupported language for {}", record.relative_path),
            ),
            cache: None,
        });
    }

    let old = match old {
        Some(cached) if cached.language == record.language && cached.hash == record.hash => {
            let mut parsed = extract_language(record.clone(), &cached.source, &cached.tree);
            parsed.changed_ranges = Vec::new();
            return Ok(ParseOutput {
                index,
                parsed,
                cache: Some(cached),
            });
        }
        other => other,
    };

    let source = match fs::read_to_string(&record.path) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
            return Ok(ParseOutput {
                index,
                parsed: ParsedFile::unsupported(
                    record.clone(),
                    format!("non-UTF-8 source for {}", record.relative_path),
                ),
                cache: None,
            });
        }
        Err(err) => return Err(err.into()),
    };

    let old = old.filter(|cached| cached.language == record.language);
    let (tree, changed_ranges) = match old {
        Some(mut cached) => {
            let edit = input_edit(&cached.source, &source);
            cached.tree.edit(&edit);
            let parser = parsers.parser_for_language(record.language)?;
            let new_tree = parser.parse(&source, Some(&cached.tree)).ok_or_else(|| {
                SqueezyError::Parse(format!(
                    "tree-sitter returned no {:?} tree",
                    record.language
                ))
            })?;
            let mut changed_ranges = cached
                .tree
                .changed_ranges(&new_tree)
                .map(span_from_range)
                .collect::<Vec<_>>();
            if changed_ranges.is_empty() {
                changed_ranges.push(span_from_edit(&edit));
            }
            (new_tree, changed_ranges)
        }
        None => {
            let parser = parsers.parser_for_language(record.language)?;
            let tree = parser.parse(&source, None).ok_or_else(|| {
                SqueezyError::Parse(format!(
                    "tree-sitter returned no {:?} tree",
                    record.language
                ))
            })?;
            (tree, Vec::new())
        }
    };

    let mut parsed = extract_language(record.clone(), &source, &tree);
    parsed.changed_ranges = changed_ranges;
    Ok(ParseOutput {
        index,
        parsed,
        cache: Some(CachedParsedFile {
            hash: record.hash.clone(),
            language: record.language,
            source,
            tree,
        }),
    })
}

fn update_parse_summary(summary: &mut ParseSummary, parsed_file: &ParsedFile) {
    if parsed_file.unsupported.is_some() {
        summary.unsupported_files += 1;
    } else {
        summary.parsed_files += 1;
    }
    if !parsed_file.changed_ranges.is_empty() {
        summary.changed_files += 1;
        summary.changed_ranges += parsed_file.changed_ranges.len();
    }
}

fn parser_with_rust_language() -> Result<Parser> {
    let mut parser = Parser::new();
    let language = rust_language();
    parser
        .set_language(&language)
        .map_err(|err| SqueezyError::Parse(format!("failed to load Rust grammar: {err}")))?;
    Ok(parser)
}

fn parser_with_go_language() -> Result<Parser> {
    let mut parser = Parser::new();
    let language = go_language();
    parser
        .set_language(&language)
        .map_err(|err| SqueezyError::Parse(format!("failed to load Go grammar: {err}")))?;
    Ok(parser)
}

fn parser_with_python_language() -> Result<Parser> {
    let mut parser = Parser::new();
    let language = python_language();
    parser
        .set_language(&language)
        .map_err(|err| SqueezyError::Parse(format!("failed to load Python grammar: {err}")))?;
    Ok(parser)
}

fn parser_with_c_language() -> Result<Parser> {
    let mut parser = Parser::new();
    let language = c_language();
    parser
        .set_language(&language)
        .map_err(|err| SqueezyError::Parse(format!("failed to load C grammar: {err}")))?;
    Ok(parser)
}

fn parser_with_cpp_language() -> Result<Parser> {
    let mut parser = Parser::new();
    let language = cpp_language();
    parser
        .set_language(&language)
        .map_err(|err| SqueezyError::Parse(format!("failed to load C++ grammar: {err}")))?;
    Ok(parser)
}

fn go_language() -> tree_sitter::Language {
    tree_sitter_go::LANGUAGE.into()
}

fn rust_language() -> tree_sitter::Language {
    tree_sitter_rust::LANGUAGE.into()
}

fn python_language() -> tree_sitter::Language {
    tree_sitter_python::LANGUAGE.into()
}

fn c_language() -> tree_sitter::Language {
    tree_sitter_c::LANGUAGE.into()
}

fn cpp_language() -> tree_sitter::Language {
    tree_sitter_cpp::LANGUAGE.into()
}

fn is_supported_language(language: LanguageKind) -> bool {
    matches!(
        language,
        LanguageKind::C
            | LanguageKind::Cpp
            | LanguageKind::Go
            | LanguageKind::Rust
            | LanguageKind::Python
    )
}

fn extract_language(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    match file.language {
        LanguageKind::C | LanguageKind::Cpp => extract_c_family(file, source, tree),
        LanguageKind::Go => extract_go(file, source, tree),
        LanguageKind::Rust => extract_rust(file, source, tree),
        LanguageKind::Python => extract_python(file, source, tree),
        _ => ParsedFile::unsupported(
            file.clone(),
            format!("unsupported language for {}", file.relative_path),
        ),
    }
}

fn extract_rust(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    let mut ctx = ExtractContext {
        file: file.clone(),
        source,
        symbols: Vec::new(),
        imports: Vec::new(),
        calls: Vec::new(),
        references: Vec::new(),
        body_hits: Vec::new(),
        diagnostics: Vec::new(),
        go_type_index: HashMap::new(),
    };
    let root = tree.root_node();
    if root.has_error() {
        ctx.diagnostics.push(ParseDiagnostic {
            message: "tree-sitter reported parse errors".to_string(),
            span: Some(span_from_node(root)),
            confidence: Confidence::Partial,
        });
    }

    visit_node(root, &mut ctx, None, None);

    ParsedFile {
        file,
        package: None,
        symbols: ctx.symbols,
        imports: ctx.imports,
        calls: ctx.calls,
        references: ctx.references,
        body_hits: ctx.body_hits,
        unsupported: None,
        diagnostics: ctx.diagnostics,
        changed_ranges: Vec::new(),
    }
}

fn extract_c_family(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    let mut ctx = ExtractContext {
        file: file.clone(),
        source,
        symbols: Vec::new(),
        imports: Vec::new(),
        calls: Vec::new(),
        references: Vec::new(),
        body_hits: Vec::new(),
        diagnostics: Vec::new(),
        go_type_index: HashMap::new(),
    };
    let root = tree.root_node();
    if root.has_error() {
        ctx.diagnostics.push(ParseDiagnostic {
            message: "tree-sitter reported parse errors".to_string(),
            span: Some(span_from_node(root)),
            confidence: Confidence::Partial,
        });
    }

    visit_c_family_node(root, &mut ctx, None, None);
    dedup_c_family_facts(&mut ctx);
    collapse_c_family_function_decls(&mut ctx);

    ParsedFile {
        file,
        package: None,
        symbols: ctx.symbols,
        imports: ctx.imports,
        calls: ctx.calls,
        references: ctx.references,
        body_hits: ctx.body_hits,
        unsupported: None,
        diagnostics: ctx.diagnostics,
        changed_ranges: Vec::new(),
    }
}

fn extract_python(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    let mut ctx = ExtractContext {
        file: file.clone(),
        source,
        symbols: Vec::new(),
        imports: Vec::new(),
        calls: Vec::new(),
        references: Vec::new(),
        body_hits: Vec::new(),
        diagnostics: Vec::new(),
        go_type_index: HashMap::new(),
    };
    let root = tree.root_node();
    if root.has_error() {
        ctx.diagnostics.push(ParseDiagnostic {
            message: "tree-sitter reported parse errors".to_string(),
            span: Some(span_from_node(root)),
            confidence: Confidence::Partial,
        });
    }

    visit_python_node(root, &mut ctx, None, None);
    extract_python_module_exports(&mut ctx);
    dedup_python_facts(&mut ctx);

    ParsedFile {
        file,
        package: None,
        symbols: ctx.symbols,
        imports: ctx.imports,
        calls: ctx.calls,
        references: ctx.references,
        body_hits: ctx.body_hits,
        unsupported: None,
        diagnostics: ctx.diagnostics,
        changed_ranges: Vec::new(),
    }
}

fn extract_go(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    let mut ctx = ExtractContext {
        file: file.clone(),
        source,
        symbols: Vec::new(),
        imports: Vec::new(),
        calls: Vec::new(),
        references: Vec::new(),
        body_hits: Vec::new(),
        diagnostics: Vec::new(),
        go_type_index: HashMap::new(),
    };
    let root = tree.root_node();
    if root.has_error() {
        ctx.diagnostics.push(ParseDiagnostic {
            message: "tree-sitter reported parse errors".to_string(),
            span: Some(span_from_node(root)),
            confidence: Confidence::Partial,
        });
    }

    let package = go_package_name(root, source);
    // Pre-scan top-level type declarations so methods declared earlier in
    // source order than their receiver type still attach to the right parent.
    // The symbol ids computed here must match the ones produced later by
    // `go_type_symbol`, so we use the same `symbol_id` inputs (file, parent=None,
    // kind, name, span).
    ctx.go_type_index = collect_go_type_index(root, &file, source);
    visit_go_node(root, &mut ctx, None, None);
    dedup_go_facts(&mut ctx);

    ParsedFile {
        file,
        package,
        symbols: ctx.symbols,
        imports: ctx.imports,
        calls: ctx.calls,
        references: ctx.references,
        body_hits: ctx.body_hits,
        unsupported: None,
        diagnostics: ctx.diagnostics,
        changed_ranges: Vec::new(),
    }
}

struct ExtractContext<'source> {
    file: FileRecord,
    source: &'source str,
    symbols: Vec<ParsedSymbol>,
    imports: Vec<ParsedImport>,
    calls: Vec<ParsedCall>,
    references: Vec<ParsedReference>,
    body_hits: Vec<BodyHit>,
    diagnostics: Vec<ParseDiagnostic>,
    go_type_index: HashMap<String, SymbolId>,
}

fn visit_go_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
    owner_symbol: Option<SymbolId>,
) {
    if node.is_missing() {
        ctx.diagnostics.push(ParseDiagnostic {
            message: format!("missing {}", node.kind()),
            span: Some(span_from_node(node)),
            confidence: Confidence::Partial,
        });
        return;
    }

    match node.kind() {
        "import_declaration" => extract_go_import(node, ctx, owner_symbol.clone()),
        "const_declaration" | "var_declaration"
            if owner_symbol.is_none() && !go_has_ancestor_kind(node, "func_literal") =>
        {
            extract_go_value_declarations(node, ctx, parent_symbol.clone());
        }
        "field_declaration" => {
            ctx.symbols
                .extend(go_field_symbols(node, ctx, parent_symbol.clone()));
        }
        _ => {}
    }

    if let Some(symbol) =
        go_symbol_from_node(node, ctx, parent_symbol.clone(), owner_symbol.as_ref())
    {
        let next_parent = Some(symbol.id.clone());
        let next_owner = if symbol.body_span.is_some()
            || matches!(
                symbol.kind,
                SymbolKind::Function | SymbolKind::Method | SymbolKind::Test
            ) {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        ctx.symbols.push(symbol);
        visit_go_children(node, ctx, next_parent, next_owner);
        return;
    }

    match node.kind() {
        "call_expression" => extract_go_call(node, ctx, owner_symbol.clone()),
        "selector_expression" => extract_go_selector_reference(node, ctx, owner_symbol.clone()),
        kind if go_reference_kind(kind).is_some() => {
            extract_go_reference(
                node,
                go_reference_kind(kind).unwrap(),
                ctx,
                owner_symbol.clone(),
            );
        }
        kind if is_go_literal(kind) => {
            extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.clone())
        }
        _ => {}
    }

    visit_go_children(node, ctx, parent_symbol, owner_symbol);
}

fn visit_go_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
    owner_symbol: Option<SymbolId>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_go_node(child, ctx, parent_symbol.clone(), owner_symbol.clone());
    }
}

fn go_package_name(root: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .find(|child| child.kind() == "package_clause")
        .and_then(|package| first_named_child_text(package, source))
}

fn go_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
    owner_symbol: Option<&SymbolId>,
) -> Option<ParsedSymbol> {
    match node.kind() {
        "function_declaration" => {
            go_function_symbol(node, ctx, SymbolKind::Function, parent_symbol)
        }
        "method_declaration" => {
            let receiver = go_receiver_type(node, ctx.source);
            let parent_id = receiver
                .as_deref()
                .and_then(|name| find_go_type_parent_id(ctx, name))
                .or(parent_symbol);
            let mut symbol = go_function_symbol(node, ctx, SymbolKind::Method, parent_id)?;
            if let Some(receiver) = receiver {
                symbol.attributes.push(format!("go:receiver:{receiver}"));
            }
            Some(symbol)
        }
        "type_alias" | "type_spec"
            if owner_symbol.is_none() && !go_has_ancestor_kind(node, "func_literal") =>
        {
            go_type_symbol(node, ctx, parent_symbol)
        }
        _ => None,
    }
}

fn go_function_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    mut kind: SymbolKind,
    parent_symbol: Option<SymbolId>,
) -> Option<ParsedSymbol> {
    let name = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(str::to_string)
        .or_else(|| first_named_child_text(node, ctx.source))
        .map(|text| text.trim().to_string())
        .filter(|text| is_go_identifier(text))?;
    if matches!(kind, SymbolKind::Function | SymbolKind::Method)
        && go_is_test_function(&ctx.file.relative_path, &name)
    {
        kind = SymbolKind::Test;
    }
    let body = node.child_by_field_name("body");
    let span = span_from_node(node);
    let body_span = body.map(span_from_node);
    let signature = signature_text(node, body, ctx.source);
    let id = symbol_id(&ctx.file, parent_symbol.as_ref(), kind, &name, span);
    let mut attributes = go_doc_and_semantic_attributes(node, ctx.source);
    if kind == SymbolKind::Test {
        attributes.push("go:test".to_string());
    }
    attributes.sort();
    attributes.dedup();

    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id: parent_symbol,
        name,
        kind,
        span,
        body_span,
        signature,
        visibility: go_visibility(node, ctx.source),
        docs: go_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-go", format!("{} declaration", node.kind())),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
    })
}

fn go_type_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
) -> Option<ParsedSymbol> {
    let name = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(str::to_string)
        .or_else(|| first_named_child_text(node, ctx.source))
        .map(|text| text.trim().to_string())
        .filter(|text| is_go_identifier(text))?;
    let type_node = node
        .child_by_field_name("type")
        .or_else(|| last_named_child(node));
    let kind = match type_node.map(|child| child.kind()) {
        Some("struct_type") => SymbolKind::Struct,
        Some("interface_type") => SymbolKind::Interface,
        _ => SymbolKind::TypeAlias,
    };
    let span = span_from_node(node);
    let body_span = type_node.map(span_from_node);
    let signature = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    let attributes = go_doc_and_semantic_attributes(node, ctx.source);
    Some(ParsedSymbol {
        id: symbol_id(&ctx.file, parent_symbol.as_ref(), kind, &name, span),
        file_id: ctx.file.id.clone(),
        parent_id: parent_symbol,
        name,
        kind,
        span,
        body_span,
        signature,
        visibility: go_visibility(node, ctx.source),
        docs: go_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-go", format!("{} declaration", node.kind())),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
    })
}

fn go_field_symbols(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
) -> Vec<ParsedSymbol> {
    let Some(parent_id) = parent_symbol else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut is_embed = false;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "field_identifier" | "identifier" => {
                if let Ok(text) = node_text(child, ctx.source) {
                    let text = text.trim();
                    if is_go_identifier(text) {
                        names.push((text.to_string(), span_from_node(child)));
                    }
                }
            }
            "type_identifier" if names.is_empty() => {
                if let Ok(text) = node_text(child, ctx.source) {
                    let text = text.trim();
                    if is_go_identifier(text) {
                        // A `type_identifier` with no preceding name token is
                        // a Go embedded field (e.g. `type Runner struct {
                        // Greeter }`), which promotes the embedded type's
                        // methods. Tag these so downstream consumers can
                        // distinguish them from named fields without parsing
                        // the receiver type themselves.
                        is_embed = true;
                        names.push((text.to_string(), span_from_node(child)));
                    }
                }
            }
            _ => {}
        }
    }
    names.sort_by_key(|left| left.1.start_byte);
    names.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);
    names
        .into_iter()
        .map(|(name, span)| {
            let mut attributes = vec!["go:field".to_string()];
            if is_embed {
                attributes.push("go:embed".to_string());
            }
            ParsedSymbol {
                id: symbol_id(&ctx.file, Some(&parent_id), SymbolKind::Field, &name, span),
                file_id: ctx.file.id.clone(),
                parent_id: Some(parent_id.clone()),
                name,
                kind: SymbolKind::Field,
                span,
                body_span: None,
                signature: node_text(node, ctx.source)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                visibility: go_visibility(node, ctx.source),
                docs: Vec::new(),
                attributes,
                provenance: Provenance::new("tree-sitter-go", "field declaration"),
                confidence: Confidence::ExactSyntax,
                freshness: Freshness::Fresh,
            }
        })
        .collect()
}

fn extract_go_value_declarations(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
) {
    let kind = if node.kind() == "const_declaration" {
        SymbolKind::Const
    } else {
        SymbolKind::Static
    };
    let mut cursor = node.walk();
    for child in go_value_specs(node, &mut cursor) {
        for (name, span) in go_names_in_spec(child, ctx.source) {
            ctx.symbols.push(ParsedSymbol {
                id: symbol_id(&ctx.file, parent_symbol.as_ref(), kind, &name, span),
                file_id: ctx.file.id.clone(),
                parent_id: parent_symbol.clone(),
                name,
                kind,
                span,
                body_span: None,
                signature: node_text(child, ctx.source)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                visibility: go_visibility(child, ctx.source),
                docs: Vec::new(),
                attributes: vec![if kind == SymbolKind::Const {
                    "go:const".to_string()
                } else {
                    "go:var".to_string()
                }],
                provenance: Provenance::new(
                    "tree-sitter-go",
                    format!("{} declaration", child.kind()),
                ),
                confidence: Confidence::ExactSyntax,
                freshness: Freshness::Fresh,
            });
        }
    }
}

fn go_value_specs<'tree>(
    node: Node<'tree>,
    cursor: &mut tree_sitter::TreeCursor<'tree>,
) -> Vec<Node<'tree>> {
    let mut specs = Vec::new();
    for child in node.named_children(cursor) {
        match child.kind() {
            "const_spec" | "var_spec" => specs.push(child),
            "const_spec_list" | "var_spec_list" => {
                let mut child_cursor = child.walk();
                specs.extend(go_value_specs(child, &mut child_cursor));
            }
            _ => {}
        }
    }
    specs
}

fn go_names_in_spec(node: Node<'_>, source: &str) -> Vec<(String, SourceSpan)> {
    let mut names = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(child.kind(), "identifier")
            && let Ok(text) = node_text(child, source)
        {
            let text = text.trim();
            if text != "_" && is_go_identifier(text) {
                names.push((text.to_string(), span_from_node(child)));
            }
            continue;
        }
        if child.kind() != "identifier" && !names.is_empty() {
            break;
        }
    }
    names
}

fn extract_go_import(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    for (path, alias, is_glob, span) in go_import_specs(node, raw, ctx.source) {
        ctx.imports.push(ParsedImport {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            path,
            alias,
            is_glob,
            is_reexport: false,
            span,
            provenance: Provenance::new("tree-sitter-go", "import declaration"),
        });
    }
}

fn go_import_specs(
    node: Node<'_>,
    raw: &str,
    source: &str,
) -> Vec<(String, Option<String>, bool, SourceSpan)> {
    let mut specs = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "import_spec"
            && child.kind() != "interpreted_string_literal"
            && child.kind() != "raw_string_literal"
        {
            continue;
        }
        let spec_text = node_text(child, source).unwrap_or_default().trim();
        if let Some((path, alias, is_glob)) = parse_go_import_spec_text(spec_text) {
            specs.push((path, alias, is_glob, span_from_node(child)));
        }
    }
    if specs.is_empty() {
        for line in raw.lines() {
            if let Some((path, alias, is_glob)) = parse_go_import_spec_text(line.trim()) {
                specs.push((path, alias, is_glob, span_from_node(node)));
            }
        }
    }
    specs
}

fn parse_go_import_spec_text(text: &str) -> Option<(String, Option<String>, bool)> {
    let text = text.trim().trim_start_matches("import").trim();
    let quote_index = text.find('"').or_else(|| text.find('`'))?;
    let quote = text.as_bytes()[quote_index] as char;
    let rest = &text[quote_index + quote.len_utf8()..];
    let close = rest.find(quote)?;
    let path = rest[..close].to_string();
    let alias_text = text[..quote_index].trim().trim_matches(['(', ')']).trim();
    let alias = match alias_text {
        "" => None,
        "." => return Some((path, None, true)),
        "_" => Some("_".to_string()),
        other if is_go_identifier(other) => Some(other.to_string()),
        _ => None,
    };
    Some((path, alias, false))
}

fn extract_go_call(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    // tree-sitter-go's `call_expression` always exposes a `function` field on
    // healthy parses. The first-named-child fallback only fires if the grammar
    // we link against ever drops or renames that field, and lets us record a
    // partial call instead of silently dropping the node.
    let Some(function_node) = node.child_by_field_name("function").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).next()
    }) else {
        return;
    };
    let target_text = node_text(function_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if target_text.is_empty() {
        return;
    }
    let receiver = receiver_from_go_call(&target_text);
    let arity = node
        .child_by_field_name("arguments")
        .or_else(|| last_named_child(node))
        .map(named_child_count)
        .unwrap_or_default();
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name: last_path_segment(&target_text),
        target_text: target_text.clone(),
        receiver: receiver.clone(),
        arity,
        kind: if receiver.is_some() {
            ParsedCallKind::Method
        } else {
            ParsedCallKind::Direct
        },
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-go", "call_expression"),
        confidence: if receiver.is_some() {
            Confidence::CandidateSet
        } else {
            Confidence::Heuristic
        },
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

fn extract_go_selector_reference(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() {
        return;
    }
    // Record the full selector text as a reference so import-aware resolution
    // can match `pkg.Fn` against an imported package alias. Body hits for the
    // operand and the trailing field are produced by `visit_go_children` when
    // it descends into this selector's identifier/field_identifier children, so
    // we intentionally avoid emitting an additional wrapper body hit here to
    // keep selector-heavy files (e.g. etcd, prometheus) from inflating the
    // body-hit index.
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: ReferenceKind::Field,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-go", "selector reference"),
    });
}

fn extract_go_reference(
    node: Node<'_>,
    kind: ReferenceKind,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() || go_keyword_like(&text) {
        return;
    }
    let body_kind = match kind {
        ReferenceKind::Identifier => BodyHitKind::Identifier,
        ReferenceKind::Type => BodyHitKind::Type,
        ReferenceKind::Path => BodyHitKind::Path,
        ReferenceKind::Field => BodyHitKind::Identifier,
        ReferenceKind::Attribute => BodyHitKind::Attribute,
    };
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: text.clone(),
        kind,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-go", format!("{} reference", node.kind())),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: body_kind,
        span: span_from_node(node),
    });
}

fn dedup_go_facts(ctx: &mut ExtractContext<'_>) {
    let mut symbols = HashSet::new();
    ctx.symbols.retain(|symbol| {
        symbols.insert(format!(
            "{}|{}|{:?}|{}",
            symbol.file_id.0, symbol.id.0, symbol.kind, symbol.name
        ))
    });
    let mut references = HashSet::new();
    ctx.references.retain(|reference| {
        references.insert(format!(
            "{}|{}|{}|{:?}",
            reference.file_id.0, reference.span.start_byte, reference.text, reference.kind
        ))
    });
    let mut body_hits = HashSet::new();
    ctx.body_hits.retain(|hit| {
        body_hits.insert(format!(
            "{}|{}|{}|{}|{:?}",
            hit.file_id.0, hit.span.start_byte, hit.span.end_byte, hit.text, hit.kind
        ))
    });
}

fn go_receiver_type(node: Node<'_>, source: &str) -> Option<String> {
    let receiver = node.child_by_field_name("receiver").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .find(|child| child.kind() == "parameter_list")
    })?;
    let raw = node_text(receiver, source).ok()?;
    let inner = raw
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim();
    let last = inner
        .split_whitespace()
        .last()
        .unwrap_or(inner)
        .trim_start_matches('*')
        .trim();
    let name = last_path_segment(last);
    is_go_identifier(&name).then_some(name)
}

fn find_go_type_parent_id(ctx: &ExtractContext<'_>, name: &str) -> Option<SymbolId> {
    // The prepass populates `ctx.go_type_index` with every top-level type
    // declaration in the file so methods declared earlier in source order
    // than their receiver type still attach to the right parent symbol.
    if let Some(id) = ctx.go_type_index.get(name) {
        return Some(id.clone());
    }
    ctx.symbols
        .iter()
        .rev()
        .find(|symbol| {
            symbol.name == name
                && matches!(
                    symbol.kind,
                    SymbolKind::Struct | SymbolKind::Interface | SymbolKind::TypeAlias
                )
        })
        .map(|symbol| symbol.id.clone())
}

fn collect_go_type_index(
    root: Node<'_>,
    file: &FileRecord,
    source: &str,
) -> HashMap<String, SymbolId> {
    let mut index = HashMap::new();
    collect_go_type_index_in(root, file, source, &mut index);
    index
}

fn collect_go_type_index_in(
    node: Node<'_>,
    file: &FileRecord,
    source: &str,
    index: &mut HashMap<String, SymbolId>,
) {
    // We only index top-level types here. Top-level for Go means siblings of
    // the `package_clause` under `source_file`, plus their nested
    // `type_declaration` -> `type_spec`/`type_alias` children. Skip anything
    // inside `func_literal` to mirror the visitor's scope filter.
    if go_has_ancestor_kind(node, "func_literal") {
        return;
    }
    if matches!(node.kind(), "type_spec" | "type_alias")
        && let Some((name, span, kind)) = go_type_index_entry(node, source)
    {
        let id = symbol_id(file, None, kind, &name, span);
        index.entry(name).or_insert(id);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_go_type_index_in(child, file, source, index);
    }
}

fn go_type_index_entry(node: Node<'_>, source: &str) -> Option<(String, SourceSpan, SymbolKind)> {
    let name = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, source).ok())
        .map(str::to_string)
        .or_else(|| first_named_child_text(node, source))
        .map(|text| text.trim().to_string())
        .filter(|text| is_go_identifier(text))?;
    let type_node = node
        .child_by_field_name("type")
        .or_else(|| last_named_child(node));
    let kind = match type_node.map(|child| child.kind()) {
        Some("struct_type") => SymbolKind::Struct,
        Some("interface_type") => SymbolKind::Interface,
        _ => SymbolKind::TypeAlias,
    };
    Some((name, span_from_node(node), kind))
}

fn go_doc_and_semantic_attributes(node: Node<'_>, source: &str) -> Vec<String> {
    let mut attributes = Vec::new();
    if !go_docs_for_node(node, source).is_empty() {
        attributes.push("go:doc".to_string());
    }
    attributes
}

fn go_docs_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let Some(parent) = node.parent() else {
        return Vec::new();
    };
    let mut docs = Vec::new();
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.end_byte() > node.start_byte() {
            break;
        }
        if matches!(child.kind(), "comment")
            && let Ok(text) = node_text(child, source)
        {
            docs.push(text.trim().to_string());
        } else if child.is_named() && !matches!(child.kind(), "comment") {
            docs.clear();
        }
    }
    docs
}

fn go_visibility(node: Node<'_>, source: &str) -> Option<String> {
    let name = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, source).ok())
        .map(str::to_string)
        .or_else(|| first_named_child_text(node, source))
        .unwrap_or_default();
    name.chars().next().map(|ch| {
        if ch.is_ascii_uppercase() {
            "exported"
        } else {
            "package"
        }
        .to_string()
    })
}

fn go_is_test_function(relative_path: &str, name: &str) -> bool {
    relative_path.ends_with("_test.go")
        && (name.starts_with("Test") || name.starts_with("Benchmark") || name.starts_with("Fuzz"))
}

fn go_has_ancestor_kind(node: Node<'_>, kind: &str) -> bool {
    let mut parent = node.parent();
    while let Some(current) = parent {
        if current.kind() == kind {
            return true;
        }
        parent = current.parent();
    }
    false
}

fn go_reference_kind(kind: &str) -> Option<ReferenceKind> {
    match kind {
        "identifier" => Some(ReferenceKind::Identifier),
        "type_identifier" | "qualified_type" | "pointer_type" => Some(ReferenceKind::Type),
        "field_identifier" => Some(ReferenceKind::Field),
        _ => None,
    }
}

fn is_go_literal(kind: &str) -> bool {
    matches!(
        kind,
        "raw_string_literal"
            | "interpreted_string_literal"
            | "int_literal"
            | "float_literal"
            | "imaginary_literal"
            | "rune_literal"
            | "true"
            | "false"
            | "nil"
    )
}

fn is_go_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && !go_keyword_like(text)
}

fn go_keyword_like(text: &str) -> bool {
    matches!(
        text,
        "break"
            | "default"
            | "func"
            | "interface"
            | "select"
            | "case"
            | "defer"
            | "go"
            | "map"
            | "struct"
            | "chan"
            | "else"
            | "goto"
            | "package"
            | "switch"
            | "const"
            | "fallthrough"
            | "if"
            | "range"
            | "type"
            | "continue"
            | "for"
            | "import"
            | "return"
            | "var"
    )
}

fn receiver_from_go_call(target_text: &str) -> Option<String> {
    target_text
        .rsplit_once('.')
        .map(|(receiver, _)| receiver.trim().to_string())
        .filter(|receiver| !receiver.is_empty())
}

fn first_named_child_text(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .next()
        .and_then(|child| node_text(child, source).ok())
        .map(|text| text.trim().to_string())
}

fn last_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).last()
}

fn extract_python_module_exports(ctx: &mut ExtractContext<'_>) {
    for line in ctx.source.lines() {
        let line = line.trim();
        // Require a word boundary after `__all__` so identifiers like
        // `__all__module = "x"` and `__all_xs = "x"` (the latter does not even
        // share the full prefix) are not matched as `__all__` assignments,
        // and so docstring/string content of the form `__all__ = ["fake"]`
        // is only accepted when it is genuinely at line start.
        let Some(rest) = line.strip_prefix("__all__") else {
            continue;
        };
        if !rest.starts_with(|ch: char| ch.is_whitespace() || ch == '=' || ch == '+') {
            continue;
        }
        let Some((_, right)) = rest.split_once('=') else {
            continue;
        };
        for exported in python_string_list_values(right) {
            ctx.imports.push(ParsedImport {
                file_id: ctx.file.id.clone(),
                owner_id: None,
                path: exported,
                alias: None,
                is_glob: false,
                is_reexport: true,
                span: SourceSpan::new(0, 0, SourcePoint::new(0, 0), SourcePoint::new(0, 0)),
                provenance: Provenance::new("tree-sitter-python", "__all__ export"),
            });
        }
    }
}

fn dedup_python_facts(ctx: &mut ExtractContext<'_>) {
    let mut imports = HashSet::new();
    ctx.imports.retain(|import| {
        imports.insert(format!(
            "{}|{:?}|{}|{:?}|{}|{}",
            import.file_id.0,
            import.owner_id.as_ref().map(|id| id.0.as_str()),
            import.path,
            import.alias,
            import.is_glob,
            import.is_reexport
        ))
    });
}

fn dedup_c_family_facts(ctx: &mut ExtractContext<'_>) {
    type ImportKey = (String, Option<String>, String, Option<String>, bool);
    type CallKey = (String, Option<String>, String, u32, u32);
    type ReferenceKey = (String, Option<String>, String, ReferenceKind, u32, u32);

    let mut symbols: HashSet<String> = HashSet::with_capacity(ctx.symbols.len());
    ctx.symbols
        .retain(|symbol| symbols.insert(symbol.id.0.clone()));

    let mut imports: HashSet<ImportKey> = HashSet::with_capacity(ctx.imports.len());
    ctx.imports.retain(|import| {
        imports.insert((
            import.file_id.0.clone(),
            import.owner_id.as_ref().map(|id| id.0.clone()),
            import.path.clone(),
            import.alias.clone(),
            import.is_reexport,
        ))
    });

    let mut calls: HashSet<CallKey> = HashSet::with_capacity(ctx.calls.len());
    ctx.calls.retain(|call| {
        calls.insert((
            call.file_id.0.clone(),
            call.caller_id.as_ref().map(|id| id.0.clone()),
            call.target_text.clone(),
            call.span.start_byte,
            call.span.end_byte,
        ))
    });

    let mut references: HashSet<ReferenceKey> = HashSet::with_capacity(ctx.references.len());
    ctx.references.retain(|reference| {
        references.insert((
            reference.file_id.0.clone(),
            reference.owner_id.as_ref().map(|id| id.0.clone()),
            reference.text.clone(),
            reference.kind,
            reference.span.start_byte,
            reference.span.end_byte,
        ))
    });
}

/// Collapse `(file, parent, kind, name)` Function/Method symbol pairs that
/// have one forward declaration and one definition into a single symbol.
/// Tree-sitter sees the forward declaration as `declaration` and the
/// definition as `function_definition`; both create independent
/// `ParsedSymbol`s with distinct spans, but clang's AST oracle reports
/// only one canonical declaration in the main translation unit. Keeping
/// the definition (or, if there is no definition, the declaration with
/// the widest signature) keeps the symbol set aligned with clang and
/// preserves the most useful span for downstream queries.
fn collapse_c_family_function_decls(ctx: &mut ExtractContext<'_>) {
    type FunctionGroupKey = (String, Option<String>, SymbolKind, String);
    let mut groups: HashMap<FunctionGroupKey, Vec<usize>> = HashMap::new();
    for (index, symbol) in ctx.symbols.iter().enumerate() {
        if !matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method) {
            continue;
        }
        groups
            .entry((
                symbol.file_id.0.clone(),
                symbol.parent_id.as_ref().map(|id| id.0.clone()),
                symbol.kind,
                symbol.name.clone(),
            ))
            .or_default()
            .push(index);
    }

    let mut drop_indexes: HashSet<usize> = HashSet::new();
    for (_, indexes) in groups {
        if indexes.len() <= 1 {
            continue;
        }
        let preferred = pick_canonical_function_symbol(&indexes, &ctx.symbols);
        for index in indexes {
            if index != preferred {
                drop_indexes.insert(index);
            }
        }
    }

    if drop_indexes.is_empty() {
        return;
    }
    let mut index = 0;
    ctx.symbols.retain(|_| {
        let keep = !drop_indexes.contains(&index);
        index += 1;
        keep
    });
}

fn pick_canonical_function_symbol(indexes: &[usize], symbols: &[ParsedSymbol]) -> usize {
    let mut best = indexes[0];
    let mut best_score = -1i64;
    for index in indexes {
        let symbol = &symbols[*index];
        let mut score = 0i64;
        if symbol.body_span.is_some() {
            score += 1_000;
        }
        score += symbol.signature.len() as i64;
        if score > best_score {
            best_score = score;
            best = *index;
        }
    }
    best
}

fn visit_c_family_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    owner_symbol: Option<&SymbolId>,
) {
    if node.is_missing() {
        ctx.diagnostics.push(ParseDiagnostic {
            message: format!("missing {}", node.kind()),
            span: Some(span_from_node(node)),
            confidence: Confidence::Partial,
        });
        return;
    }

    let kind = node.kind();
    if kind == "preproc_include" {
        extract_c_include(node, ctx, owner_symbol.cloned());
    } else if matches!(kind, "using_declaration" | "using_directive") {
        extract_c_using(node, ctx, owner_symbol.cloned());
    }

    if let Some(symbol) = c_family_symbol_from_node(node, ctx, parent_symbol) {
        extract_c_family_symbol_facts(node, &symbol, ctx);
        let symbol_pair = (symbol.id.clone(), symbol.kind);
        let next_parent_owned = if c_family_symbol_can_own_children(symbol.kind) {
            Some(symbol_pair)
        } else {
            None
        };
        let next_owner_owned = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            None
        };
        ctx.symbols.push(symbol);
        let next_parent = next_parent_owned.as_ref().or(parent_symbol);
        let next_owner = next_owner_owned.as_ref().or(owner_symbol);
        visit_c_family_children(node, ctx, next_parent, next_owner);
        return;
    }

    if kind == "call_expression" {
        extract_c_family_call(node, ctx, owner_symbol.cloned());
    } else if matches!(kind, "preproc_call" | "preproc_function_def") {
        extract_c_macro_call(node, ctx, owner_symbol.cloned());
    } else if let Some(reference_kind) = c_family_reference_kind(node) {
        extract_c_family_reference(node, reference_kind, ctx, owner_symbol.cloned());
    } else if is_c_family_literal(kind) {
        extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.cloned());
    }

    visit_c_family_children(node, ctx, parent_symbol, owner_symbol);
}

fn visit_c_family_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    owner_symbol: Option<&SymbolId>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_c_family_node(child, ctx, parent_symbol, owner_symbol);
    }
}

fn c_family_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let mut kind = match node.kind() {
        "namespace_definition" => SymbolKind::Module,
        "class_specifier" => SymbolKind::Class,
        "struct_specifier" => SymbolKind::Struct,
        "union_specifier" => SymbolKind::Union,
        "enum_specifier" => SymbolKind::Enum,
        "enumerator" => SymbolKind::Variant,
        "function_definition" => SymbolKind::Function,
        "declaration" if c_declaration_is_function(node) => SymbolKind::Function,
        "field_declaration" if c_declaration_is_function(node) => SymbolKind::Function,
        "field_declaration" => SymbolKind::Field,
        "type_definition" | "alias_declaration" => SymbolKind::TypeAlias,
        "preproc_def" | "preproc_function_def" => SymbolKind::Macro,
        _ => return None,
    };
    if kind == SymbolKind::Function
        && parent_symbol
            .map(|(_, parent_kind)| c_family_symbol_can_own_members(*parent_kind))
            .unwrap_or(false)
    {
        kind = SymbolKind::Method;
    }
    if kind == SymbolKind::Function
        && c_family_function_declarator_qualifier(node, ctx.source)
            .as_deref()
            .map(qualifier_is_type_like)
            .unwrap_or(false)
    {
        kind = SymbolKind::Method;
    }

    let name = c_family_symbol_name(node, kind, ctx.source)?;
    if name.is_empty() {
        return None;
    }
    let body = c_family_body_node(node);
    let span = span_from_node(node);
    let body_span = body.map(span_from_node);
    let signature = signature_text(node, body, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let mut attributes = c_family_attributes_for_node(node, kind, &signature);
    attributes.sort();
    attributes.dedup();
    let confidence = c_family_symbol_confidence(node, &attributes);
    Some(ParsedSymbol {
        id: symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span),
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind,
        span,
        body_span,
        signature,
        visibility: c_family_visibility_text(node, ctx.source),
        docs: Vec::new(),
        attributes,
        provenance: Provenance::new(
            c_family_parser_name(ctx.file.language),
            format!("{} declaration", node.kind()),
        ),
        confidence,
        freshness: Freshness::Fresh,
    })
}

fn extract_c_family_symbol_facts(
    node: Node<'_>,
    symbol: &ParsedSymbol,
    ctx: &mut ExtractContext<'_>,
) {
    if matches!(symbol.kind, SymbolKind::Class | SymbolKind::Struct)
        && let Some(bases) = node.child_by_field_name("superclasses")
    {
        let mut cursor = bases.walk();
        for base in bases.named_children(&mut cursor) {
            if let Ok(text) = node_text(base, ctx.source) {
                let name = c_family_last_name(text);
                if !name.is_empty() {
                    ctx.references.push(ParsedReference {
                        file_id: ctx.file.id.clone(),
                        owner_id: Some(symbol.id.clone()),
                        text: name,
                        kind: ReferenceKind::Type,
                        span: span_from_node(base),
                        provenance: Provenance::new(
                            c_family_parser_name(ctx.file.language),
                            "base class reference",
                        ),
                    });
                }
            }
        }
    }

    if matches!(
        symbol.kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Field | SymbolKind::TypeAlias
    ) {
        for type_name in c_family_type_names_from_signature(&symbol.signature) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: type_name.clone(),
                kind: ReferenceKind::Type,
                span: symbol.span,
                provenance: Provenance::new(
                    c_family_parser_name(ctx.file.language),
                    "signature type reference",
                ),
            });
            ctx.body_hits.push(BodyHit {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: type_name,
                kind: BodyHitKind::Type,
                span: symbol.span,
            });
        }
    }
}

fn extract_c_include(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let Some(path) = c_include_path(raw) else {
        return;
    };
    // `#include "x.h"` exposes every declaration in `x.h` to the including
    // file the same way Rust's `use module::*;` does. Marking the import as
    // a glob lets `add_import_edges` and the call resolver consult the
    // include for cross-TU lookups without inventing a name match.
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id,
        path,
        alias: None,
        is_glob: true,
        is_reexport: false,
        span: span_from_node(node),
        provenance: Provenance::new(c_family_parser_name(ctx.file.language), "include directive"),
    });
}

fn c_include_path(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let start = raw.find(['"', '<'])?;
    let opener = raw.as_bytes()[start] as char;
    let closer = if opener == '"' { '"' } else { '>' };
    let rest = &raw[start + opener.len_utf8()..];
    let end = rest.find(closer)?;
    let path = rest[..end].trim();
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

/// Index `using ns::Name;` (declaration) and `using namespace ns;`
/// (directive) so cross-namespace references and calls in real C++ code can
/// resolve via the same import machinery that handles Rust `use`. Plain
/// `using` aliases like `using It = Vec::iterator;` are folded into Squeezy
/// as type-alias symbols by the symbol path, so we only emit imports for
/// the namespace-scoping forms here.
fn extract_c_using(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let trimmed = raw.trim().trim_end_matches(';').trim();
    let Some(rest) = trimmed.strip_prefix("using") else {
        return;
    };
    let rest = rest.trim();
    let is_namespace = rest.starts_with("namespace");
    let body = if is_namespace {
        rest.trim_start_matches("namespace").trim()
    } else {
        rest
    };
    if body.is_empty() || body.contains('=') {
        return;
    }
    let path = body.replace([' ', '\t', '\n'], "");
    if path.is_empty() {
        return;
    }
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id,
        path,
        alias: None,
        is_glob: is_namespace,
        is_reexport: false,
        span: span_from_node(node),
        provenance: Provenance::new(
            c_family_parser_name(ctx.file.language),
            if is_namespace {
                "using namespace directive"
            } else {
                "using declaration"
            },
        ),
    });
}

fn extract_c_family_call(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let Some(function_node) = node.child_by_field_name("function").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).next()
    }) else {
        return;
    };
    let target_text = node_text(function_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if target_text.is_empty() {
        return;
    }
    let name = c_family_last_name(&target_text);
    if name.is_empty() {
        return;
    }
    let receiver = c_family_receiver_from_call_target(&target_text);
    let arity = node
        .child_by_field_name("arguments")
        .or_else(|| {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() == "argument_list")
        })
        .map(named_child_count)
        .unwrap_or_default();
    let kind = if receiver.is_some() {
        ParsedCallKind::Method
    } else {
        ParsedCallKind::Direct
    };
    let confidence = if c_family_call_is_macro_like(&name) {
        Confidence::MacroOpaque
    } else if receiver.is_some() || target_text.contains('<') {
        Confidence::CandidateSet
    } else {
        Confidence::Heuristic
    };
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: target_text.clone(),
        receiver,
        arity,
        kind,
        span: span_from_node(node),
        provenance: Provenance::new(c_family_parser_name(ctx.file.language), "call_expression"),
        confidence,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

fn extract_c_macro_call(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let name = raw
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .split('(')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        return;
    }
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: raw.trim().to_string(),
        receiver: None,
        arity: 0,
        kind: ParsedCallKind::Macro,
        span: span_from_node(node),
        provenance: Provenance::new(
            c_family_parser_name(ctx.file.language),
            "preprocessor macro",
        ),
        confidence: Confidence::MacroOpaque,
    });
    extract_body_hit(node, BodyHitKind::Macro, ctx, owner_id);
}

fn extract_c_family_reference(
    node: Node<'_>,
    kind: ReferenceKind,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    if c_family_node_is_declaration_name(node) {
        return;
    }
    let text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() || c_family_builtin_type(&text) {
        return;
    }
    let text = match kind {
        ReferenceKind::Path | ReferenceKind::Type | ReferenceKind::Field => {
            c_family_last_name(&text)
        }
        _ => text,
    };
    if text.is_empty() {
        return;
    }
    let body_kind = match kind {
        ReferenceKind::Identifier => BodyHitKind::Identifier,
        ReferenceKind::Type => BodyHitKind::Type,
        ReferenceKind::Path => BodyHitKind::Path,
        ReferenceKind::Field => BodyHitKind::Identifier,
        ReferenceKind::Attribute => BodyHitKind::Attribute,
    };
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: text.clone(),
        kind,
        span: span_from_node(node),
        provenance: Provenance::new(
            c_family_parser_name(ctx.file.language),
            format!("{} reference", node.kind()),
        ),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: body_kind,
        span: span_from_node(node),
    });
}

fn c_family_symbol_can_own_children(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Union
            | SymbolKind::Enum
            | SymbolKind::Module
    )
}

fn c_family_symbol_can_own_members(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Union
    )
}

fn c_family_parser_name(language: LanguageKind) -> &'static str {
    match language {
        LanguageKind::C => "tree-sitter-c",
        LanguageKind::Cpp => "tree-sitter-cpp",
        _ => "tree-sitter-c-family",
    }
}

fn c_declaration_is_function(node: Node<'_>) -> bool {
    node.child_by_field_name("declarator")
        .map(c_declarator_is_real_function)
        .unwrap_or(false)
}

/// Returns true when the declarator describes a real function/method, not a
/// function pointer field/variable.
///
/// Tree-sitter wraps function pointers in a `function_declarator` whose own
/// `declarator` is a `parenthesized_declarator` containing a
/// `pointer_declarator` (`int (*cb)(int)`). Real functions wrap the
/// function_declarator around a plain identifier-like child
/// (`int helper(int)` → `function_declarator > identifier`). Clang's AST
/// oracle reports the first shape as `FieldDecl`, so we must keep them as
/// Squeezy `Field` symbols to avoid inflating FP against the oracle.
fn c_declarator_is_real_function(node: Node<'_>) -> bool {
    match node.kind() {
        "function_declarator" => node
            .child_by_field_name("declarator")
            .map(c_declarator_inner_is_function_name)
            .unwrap_or(false),
        "reference_declarator" | "init_declarator" => node
            .child_by_field_name("declarator")
            .or_else(|| first_named_child(node))
            .map(c_declarator_is_real_function)
            .unwrap_or(false),
        // `pointer_declarator`, `parenthesized_declarator`,
        // `array_declarator`, plain identifiers, anything else: not a
        // direct function declaration.
        _ => false,
    }
}

/// True when a function_declarator's inner declarator is a name-shaped node
/// (identifier, field_identifier, qualified_identifier, destructor_name,
/// operator_name). False for parenthesized/pointer declarators that signal
/// function pointers.
fn c_declarator_inner_is_function_name(node: Node<'_>) -> bool {
    match node.kind() {
        "identifier"
        | "field_identifier"
        | "type_identifier"
        | "qualified_identifier"
        | "namespace_identifier"
        | "destructor_name"
        | "operator_name"
        | "template_function" => true,
        // Reference declarators wrap a single inner declarator; rare but
        // legitimate for ref-qualified function returns. Recurse.
        "reference_declarator" => node
            .child_by_field_name("declarator")
            .or_else(|| first_named_child(node))
            .map(c_declarator_inner_is_function_name)
            .unwrap_or(false),
        _ => false,
    }
}

fn first_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

fn c_family_symbol_name(node: Node<'_>, kind: SymbolKind, source: &str) -> Option<String> {
    match kind {
        SymbolKind::Macro => c_macro_definition_name(node, source),
        SymbolKind::TypeAlias => c_type_alias_name(node, source),
        SymbolKind::Field => c_declarator_name(node, source),
        SymbolKind::Function | SymbolKind::Method => node
            .child_by_field_name("declarator")
            .and_then(|declarator| c_declarator_name(declarator, source))
            .or_else(|| c_declarator_name(node, source)),
        _ => node
            .child_by_field_name("name")
            .and_then(|child| node_text(child, source).ok())
            .map(c_family_last_name)
            .or_else(|| c_named_child_text(node, source)),
    }
}

/// Returns the qualifier prefix of a function declarator (e.g. `Foo::bar`
/// → `Some("Foo")`, `ns::free_function` → `Some("ns")`, `free` → `None`).
/// Used to distinguish out-of-line method definitions (`void Foo::bar()`)
/// from namespace-qualified free functions (`void ns::func()`) without a
/// second pass over the symbol table.
fn c_family_function_declarator_qualifier(node: Node<'_>, source: &str) -> Option<String> {
    let declarator = node.child_by_field_name("declarator")?;
    let text = node_text(declarator, source).ok()?;
    let head = text.split('(').next().unwrap_or(text).trim();
    let (qualifier, _) = head.rsplit_once("::")?;
    let qualifier = qualifier
        .trim()
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != ':');
    if qualifier.is_empty() {
        None
    } else {
        Some(qualifier.to_string())
    }
}

/// Apply `looks_like_type_name` to the last segment of a `::`-qualifier.
/// Class names follow the type-name convention (uppercase initial or `_t`
/// suffix), namespace identifiers do not. This is a cheap heuristic; a
/// post-pass that walks symbols can later upgrade ambiguous cases.
fn qualifier_is_type_like(qualifier: &str) -> bool {
    let leaf = qualifier.rsplit("::").next().unwrap_or(qualifier).trim();
    !leaf.is_empty() && looks_like_type_name(leaf)
}

fn c_named_child_text(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| {
            matches!(
                child.kind(),
                "identifier"
                    | "type_identifier"
                    | "field_identifier"
                    | "qualified_identifier"
                    | "namespace_identifier"
                    | "destructor_name"
                    | "operator_name"
            )
        })
        .and_then(|child| node_text(child, source).ok())
        .map(c_family_last_name)
}

fn c_declarator_name(node: Node<'_>, source: &str) -> Option<String> {
    if matches!(
        node.kind(),
        "identifier"
            | "field_identifier"
            | "type_identifier"
            | "qualified_identifier"
            | "namespace_identifier"
            | "destructor_name"
            | "operator_name"
    ) {
        return node_text(node, source).ok().map(c_family_last_name);
    }
    if let Some(name) = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, source).ok())
        .map(c_family_last_name)
        .filter(|name| !name.is_empty())
    {
        return Some(name);
    }
    if let Some(name) = node
        .child_by_field_name("declarator")
        .and_then(|child| c_declarator_name(child, source))
    {
        return Some(name);
    }
    let mut cursor = node.walk();
    let children = node.named_children(&mut cursor).collect::<Vec<_>>();
    for child in children.into_iter().rev() {
        if matches!(
            child.kind(),
            "parameter_list"
                | "field_declaration_list"
                | "argument_list"
                | "template_argument_list"
                | "template_parameter_list"
        ) {
            continue;
        }
        if let Some(name) = c_declarator_name(child, source).filter(|name| !name.is_empty()) {
            return Some(name);
        }
    }
    None
}

fn c_type_alias_name(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("declarator")
        .and_then(|child| c_declarator_name(child, source))
        .or_else(|| {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .filter(|child| matches!(child.kind(), "type_identifier" | "identifier"))
                .filter_map(|child| node_text(child, source).ok())
                .map(c_family_last_name)
                .last()
        })
}

fn c_macro_definition_name(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|child| node_text(child, source).ok())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let raw = node_text(node, source).ok()?;
            raw.split_whitespace()
                .nth(1)
                .and_then(|name| name.split('(').next())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
}

fn c_family_body_node(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("body").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).find(|child| {
            matches!(
                child.kind(),
                "compound_statement"
                    | "field_declaration_list"
                    | "enumerator_list"
                    | "declaration_list"
            )
        })
    })
}

fn c_family_attributes_for_node(node: Node<'_>, kind: SymbolKind, signature: &str) -> Vec<String> {
    let mut attributes = Vec::new();
    match node.kind() {
        "function_definition" => attributes.push("c-family:definition".to_string()),
        "declaration" if matches!(kind, SymbolKind::Function | SymbolKind::Method) => {
            attributes.push("c-family:declaration".to_string())
        }
        "field_declaration" if matches!(kind, SymbolKind::Function | SymbolKind::Method) => {
            attributes.push("c-family:declaration".to_string())
        }
        "field_declaration" => attributes.push("c-family:field".to_string()),
        "enumerator" => attributes.push("c-family:enum-variant".to_string()),
        "preproc_def" | "preproc_function_def" => {
            attributes.push("c-family:macro".to_string());
            attributes.push("preprocessor:opaque".to_string());
        }
        "template_declaration" => attributes.push("c++:template".to_string()),
        _ => {}
    }

    let ancestors = c_family_ancestor_kinds(node);
    if ancestors.template {
        attributes.push("c++:template".to_string());
    }
    if ancestors.conditional {
        attributes.push("preprocessor:conditional".to_string());
    }

    // `virtual` is only meaningful for function/method symbols, and the
    // signature slice is already start..body_start so the search avoids
    // scanning the full class body.
    if matches!(
        kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Field
    ) && signature_has_keyword(signature, "virtual")
    {
        attributes.push("c++:virtual".to_string());
    }

    if matches!(
        kind,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Union
    ) && c_family_is_template_specialization(node)
    {
        attributes.push("c++:template-specialization".to_string());
    }

    attributes
}

#[derive(Default)]
struct CFamilyAncestorFlags {
    template: bool,
    conditional: bool,
}

/// Single ancestor walk that records every kind we care about so attribute
/// extraction doesn't repeat the same parent walk for each flag.
fn c_family_ancestor_kinds(node: Node<'_>) -> CFamilyAncestorFlags {
    let mut flags = CFamilyAncestorFlags::default();
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "template_declaration" => flags.template = true,
            "preproc_if" | "preproc_ifdef" | "preproc_ifndef" => flags.conditional = true,
            _ => {}
        }
        current = parent.parent();
    }
    flags
}

/// True when the given identifier appears as a whole-token keyword in the
/// signature slice. Avoids substring matches like `nonvirtual` or strings
/// embedded in default parameter values.
fn signature_has_keyword(signature: &str, keyword: &str) -> bool {
    for token in signature.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_') {
        if token == keyword {
            return true;
        }
    }
    false
}

/// Detect `template<> class Foo<int> { … }` and `template<typename T> class
/// Foo<int, T> {}` shaped specializations. Tree-sitter-cpp represents these
/// as `class_specifier` whose `name` field is a `template_type` rather than a
/// `type_identifier`, with the explicit template arg list nested inside.
fn c_family_is_template_specialization(node: Node<'_>) -> bool {
    let Some(name) = node.child_by_field_name("name") else {
        return false;
    };
    name.kind() == "template_type"
}

fn c_family_symbol_confidence(node: Node<'_>, attributes: &[String]) -> Confidence {
    if attributes
        .iter()
        .any(|attribute| attribute == "preprocessor:opaque")
    {
        return Confidence::MacroOpaque;
    }
    if attributes
        .iter()
        .any(|attribute| attribute == "preprocessor:conditional")
    {
        return Confidence::ConditionalUnknown;
    }
    if attributes
        .iter()
        .any(|attribute| attribute == "c++:template-specialization" || attribute == "c++:template")
    {
        return Confidence::Partial;
    }
    if node.kind() == "declaration" {
        return Confidence::Heuristic;
    }
    Confidence::ExactSyntax
}

/// Resolve the C++ access modifier for a class/struct member.
///
/// tree-sitter-cpp models `public:` / `private:` / `protected:` as keyword
/// children of an `access_specifier` named node. Walking prev_named_siblings
/// finds the closest preceding `access_specifier`; if none exists, the
/// containing aggregate's default applies (`struct` → public, `class` →
/// private, `union` → public). For non-member symbols we still fall back to
/// the leading-keyword scan so `static int g;` reports `static`.
fn c_family_visibility_text(node: Node<'_>, source: &str) -> Option<String> {
    let mut sibling = node.prev_named_sibling();
    while let Some(current) = sibling {
        if current.kind() == "access_specifier"
            && let Some(keyword) = c_family_access_specifier_keyword(current, source)
        {
            return Some(keyword);
        }
        sibling = current.prev_named_sibling();
    }

    if let Some(parent) = node.parent()
        && parent.kind() == "field_declaration_list"
        && let Some(default) = c_family_aggregate_default_access(parent)
    {
        return Some(default.to_string());
    }

    let raw = node_text(node, source).ok()?.trim_start();
    [
        "static",
        "extern",
        "inline",
        "public",
        "private",
        "protected",
    ]
    .into_iter()
    .find(|keyword| {
        raw.starts_with(*keyword)
            && raw
                .as_bytes()
                .get(keyword.len())
                .is_none_or(|byte| !((*byte as char).is_ascii_alphanumeric() || *byte == b'_'))
    })
    .map(str::to_string)
}

fn c_family_access_specifier_keyword(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "public" | "private" | "protected") {
            return Some(child.kind().to_string());
        }
    }
    // Fallback: scan the text. `access_specifier` may report the keyword as
    // an anonymous child on some grammar versions.
    let raw = node_text(node, source).ok()?.trim();
    ["public", "private", "protected"]
        .into_iter()
        .find(|keyword| raw.starts_with(*keyword))
        .map(str::to_string)
}

fn c_family_aggregate_default_access(field_list: Node<'_>) -> Option<&'static str> {
    let parent = field_list.parent()?;
    match parent.kind() {
        "class_specifier" => Some("private"),
        "struct_specifier" | "union_specifier" => Some("public"),
        _ => None,
    }
}

fn c_family_reference_kind(node: Node<'_>) -> Option<ReferenceKind> {
    match node.kind() {
        "identifier" => Some(ReferenceKind::Identifier),
        "type_identifier" | "primitive_type" | "sized_type_specifier" => Some(ReferenceKind::Type),
        "qualified_identifier" | "scoped_identifier" | "namespace_identifier" => {
            Some(ReferenceKind::Path)
        }
        "field_identifier" => Some(ReferenceKind::Field),
        "attribute_specifier" | "attribute_declaration" => Some(ReferenceKind::Attribute),
        _ => None,
    }
}

fn c_family_node_is_declaration_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent
        .child_by_field_name("name")
        .map(|name| name.id() == node.id())
        .unwrap_or(false)
    {
        return true;
    }
    if matches!(
        parent.kind(),
        "function_declarator" | "pointer_declarator" | "reference_declarator" | "init_declarator"
    ) && parent
        .child_by_field_name("declarator")
        .map(|declarator| declarator.id() == node.id())
        .unwrap_or(false)
    {
        return true;
    }
    matches!(
        parent.kind(),
        "struct_specifier"
            | "class_specifier"
            | "union_specifier"
            | "enum_specifier"
            | "enumerator"
            | "type_definition"
            | "alias_declaration"
            | "namespace_definition"
            | "preproc_def"
            | "preproc_function_def"
    )
}

fn c_family_type_names_from_signature(signature: &str) -> Vec<String> {
    let mut names = Vec::new();
    for token in signature
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == ':' || ch == '~'))
    {
        let name = c_family_last_name(token);
        if name.is_empty() || c_family_builtin_type(&name) || !looks_like_type_name(&name) {
            continue;
        }
        names.push(name);
    }
    names.sort();
    names.dedup();
    names
}

fn c_family_last_name(path: &str) -> String {
    path.trim()
        .trim_matches(|ch: char| {
            matches!(
                ch,
                '&' | '*' | '(' | ')' | '[' | ']' | '{' | '}' | ';' | ',' | ':' | '<' | '>'
            )
        })
        .rsplit("::")
        .next()
        .unwrap_or(path)
        .rsplit("->")
        .next()
        .unwrap_or(path)
        .rsplit('.')
        .next()
        .unwrap_or(path)
        .trim()
        .trim_start_matches('~')
        .to_string()
}

fn c_family_receiver_from_call_target(target_text: &str) -> Option<String> {
    target_text
        .rsplit_once("::")
        .or_else(|| target_text.rsplit_once("->"))
        .or_else(|| target_text.rsplit_once('.'))
        .map(|(receiver, _)| receiver.trim().to_string())
        .filter(|receiver| !receiver.is_empty())
}

/// True when the call target reads like an all-caps preprocessor macro.
///
/// We're lenient: anything that contains zero lowercase ASCII letters and is
/// at least two characters long (so single-letter identifiers like `N`
/// don't fire) is treated as macro-like. This catches both `EXPECT_EQ` and
/// underscore-free names like `ASSERT`, `LOG`, and `CHECK`. The body
/// extractor still records the literal call site, so over-flagging only
/// widens the macro-opaque cone — it never invents calls.
fn c_family_call_is_macro_like(name: &str) -> bool {
    if name.len() < 2 {
        return false;
    }
    let mut has_alpha = false;
    for ch in name.chars() {
        if ch.is_ascii_lowercase() {
            return false;
        }
        if ch.is_ascii_alphabetic() {
            has_alpha = true;
        }
    }
    has_alpha
}

fn c_family_builtin_type(text: &str) -> bool {
    matches!(
        text,
        "auto"
            | "bool"
            | "char"
            | "const"
            | "double"
            | "extern"
            | "float"
            | "inline"
            | "int"
            | "long"
            | "mutable"
            | "register"
            | "restrict"
            | "short"
            | "signed"
            | "size_t"
            | "static"
            | "struct"
            | "template"
            | "typename"
            | "union"
            | "unsigned"
            | "void"
            | "volatile"
    )
}

fn looks_like_type_name(name: &str) -> bool {
    name.chars()
        .next()
        .map(|ch| ch.is_ascii_uppercase())
        .unwrap_or(false)
        || name.ends_with("_t")
}

fn is_c_family_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal"
            | "raw_string_literal"
            | "number_literal"
            | "char_literal"
            | "true"
            | "false"
            | "null"
            | "nullptr"
    )
}

fn visit_python_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
) {
    if node.is_missing() {
        ctx.diagnostics.push(ParseDiagnostic {
            message: format!("missing {}", node.kind()),
            span: Some(span_from_node(node)),
            confidence: Confidence::Partial,
        });
        return;
    }

    let kind = node.kind();
    if matches!(kind, "import_statement" | "import_from_statement") {
        extract_python_import(node, ctx, owner_symbol.clone());
    }

    if let Some(symbol) = python_symbol_from_node(node, ctx, parent_symbol.as_ref()) {
        extract_python_symbol_facts(node, &symbol, ctx);
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        ctx.symbols.push(symbol);
        visit_python_children(node, ctx, next_parent, next_owner);
        return;
    }

    if kind == "call" && !python_node_is_inside_decorator(node) {
        extract_python_call(node, ctx, owner_symbol.clone());
    } else if matches!(kind, "assignment" | "assignment_statement") {
        extract_python_field_symbol(node, ctx, parent_symbol.as_ref());
        extract_python_assignment(node, ctx, owner_symbol.clone());
    } else if kind == "identifier" {
        extract_python_reference(node, ReferenceKind::Identifier, ctx, owner_symbol.clone());
    } else if kind == "attribute" {
        extract_python_reference(node, ReferenceKind::Field, ctx, owner_symbol.clone());
    } else if is_python_literal(kind) {
        extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.clone());
    }

    visit_python_children(node, ctx, parent_symbol, owner_symbol);
}

fn visit_python_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_python_node(child, ctx, parent_symbol.clone(), owner_symbol.clone());
    }
}

fn visit_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
    owner_symbol: Option<SymbolId>,
) {
    if node.is_missing() {
        ctx.diagnostics.push(ParseDiagnostic {
            message: format!("missing {}", node.kind()),
            span: Some(span_from_node(node)),
            confidence: Confidence::Partial,
        });
        return;
    }

    let kind = node.kind();
    if kind == "use_declaration" {
        extract_import(node, ctx, owner_symbol.clone());
    }

    if let Some(symbol) = symbol_from_node(node, ctx, parent_symbol.clone()) {
        let next_parent = Some(symbol.id.clone());
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        ctx.symbols.push(symbol);
        visit_children(node, ctx, next_parent, next_owner);
        return;
    }

    if kind == "call_expression" {
        extract_direct_call(node, ctx, owner_symbol.clone());
    } else if kind == "method_call_expression" {
        extract_method_call(node, ctx, owner_symbol.clone());
    } else if kind == "macro_invocation" {
        extract_macro_call(node, ctx, owner_symbol.clone());
    } else if let Some(reference_kind) = reference_kind(kind) {
        extract_reference(node, reference_kind, ctx, owner_symbol.clone());
    } else if is_literal(kind) {
        extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.clone());
    }

    visit_children(node, ctx, parent_symbol, owner_symbol);
}

fn visit_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
    owner_symbol: Option<SymbolId>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_node(child, ctx, parent_symbol.clone(), owner_symbol.clone());
    }
}

fn symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
) -> Option<ParsedSymbol> {
    let mut kind = match node.kind() {
        "mod_item" => SymbolKind::Module,
        "struct_item" => SymbolKind::Struct,
        "enum_item" => SymbolKind::Enum,
        "union_item" => SymbolKind::Union,
        "trait_item" => SymbolKind::Trait,
        "impl_item" => SymbolKind::Impl,
        "function_item" | "function_signature_item" => SymbolKind::Function,
        "const_item" => SymbolKind::Const,
        "static_item" => SymbolKind::Static,
        "type_item" | "associated_type" => SymbolKind::TypeAlias,
        "macro_definition" => SymbolKind::Macro,
        _ => return None,
    };

    if kind == SymbolKind::Function
        && parent_symbol_is_impl_or_trait(&parent_symbol)
        && function_has_self_parameter(node, ctx.source)
    {
        kind = SymbolKind::Method;
    }

    let attributes = attributes_for_node(node, ctx.source);
    if kind == SymbolKind::Function && is_test_function(&attributes) {
        kind = SymbolKind::Test;
    }

    let name = symbol_name(node, kind, ctx.source)?;
    if kind == SymbolKind::Const && name == "_" {
        return None;
    }
    let body = node.child_by_field_name("body");
    let span = span_from_node(node);
    let body_span = body.map(span_from_node);
    let signature = signature_text(node, body, ctx.source);
    let visibility = visibility_text(node, ctx.source);
    let id = symbol_id(&ctx.file, parent_symbol.as_ref(), kind, &name, span);

    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id: parent_symbol,
        name,
        kind,
        span,
        body_span,
        signature,
        visibility,
        docs: docs_from_attributes(&attributes),
        attributes,
        provenance: Provenance::new("tree-sitter-rust", format!("{} declaration", node.kind())),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
    })
}

fn python_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let mut kind = match node.kind() {
        "class_definition" => SymbolKind::Class,
        "function_definition" => SymbolKind::Function,
        _ => return None,
    };
    if kind == SymbolKind::Function
        && parent_symbol
            .map(|(_, parent_kind)| *parent_kind == SymbolKind::Class)
            .unwrap_or(false)
    {
        kind = SymbolKind::Method;
    }

    let name = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())?;
    let body = node.child_by_field_name("body");
    let span = span_from_node(node);
    let body_span = body.map(span_from_node);
    let signature = signature_text(node, body, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let id = symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span);
    let mut attributes = python_attributes_for_node(node, ctx.source);
    if kind == SymbolKind::Class {
        attributes.extend(
            python_class_bases(&signature)
                .into_iter()
                .map(|base| format!("base:{base}")),
        );
    }
    let docs = python_docs_for_node(node, ctx.source);
    attributes.sort();
    attributes.dedup();
    attributes.extend(python_test_attributes(&ctx.file.relative_path, kind, &name));
    attributes.sort();
    attributes.dedup();

    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind,
        span,
        body_span,
        signature,
        visibility: None,
        docs,
        attributes,
        provenance: Provenance::new("tree-sitter-python", format!("{} declaration", node.kind())),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
    })
}

fn extract_python_symbol_facts(
    node: Node<'_>,
    symbol: &ParsedSymbol,
    ctx: &mut ExtractContext<'_>,
) {
    if symbol.kind == SymbolKind::Class {
        for base in python_class_bases(&symbol.signature) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: base,
                kind: ReferenceKind::Type,
                span: symbol.span,
                provenance: Provenance::new("tree-sitter-python", "class base reference"),
            });
        }
    }

    if matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method) {
        for annotation in python_type_annotations(&symbol.signature) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: annotation.clone(),
                kind: ReferenceKind::Type,
                span: symbol.span,
                provenance: Provenance::new("tree-sitter-python", "type annotation reference"),
            });
            ctx.body_hits.push(BodyHit {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: annotation,
                kind: BodyHitKind::Type,
                span: symbol.span,
            });
        }
    }

    let _ = node;
}

fn python_attributes_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let Some(parent) = node.parent() else {
        return Vec::new();
    };
    if parent.kind() != "decorated_definition" {
        return Vec::new();
    }
    let mut cursor = parent.walk();
    let mut attributes = parent
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "decorator")
        .filter_map(|child| node_text(child, source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();

    let mut semantic = attributes
        .iter()
        .flat_map(|attribute| python_semantic_attributes(attribute))
        .collect::<Vec<_>>();
    attributes.append(&mut semantic);
    attributes.sort();
    attributes.dedup();
    attributes
}

fn python_semantic_attributes(attribute: &str) -> Vec<String> {
    let trimmed = attribute.trim().trim_start_matches('@').trim();
    let target = trimmed
        .split('(')
        .next()
        .unwrap_or(trimmed)
        .trim()
        .trim_end_matches('.');
    let leaf = target.rsplit('.').next().unwrap_or(target);
    let mut attributes = Vec::new();
    match leaf {
        "property" | "staticmethod" | "classmethod" => {
            attributes.push(format!("python:{leaf}"));
        }
        "dataclass" => attributes.push("python:dataclass".to_string()),
        "fixture" => attributes.push("pytest:fixture".to_string()),
        "validator" | "field_validator" | "model_validator" => {
            attributes.push(format!("pydantic:{leaf}"));
        }
        "get" | "post" | "put" | "patch" | "delete" | "options" | "head" | "route" => {
            let receiver = target.rsplit_once('.').map(|(receiver, _)| receiver);
            if receiver
                .map(|receiver| {
                    matches!(
                        receiver.rsplit('.').next().unwrap_or(receiver),
                        "app" | "router" | "blueprint" | "bp"
                    )
                })
                .unwrap_or(false)
            {
                let method = leaf.to_ascii_uppercase();
                attributes.push(format!("route:{method}"));
                if let Some(path) = first_python_string_literal(attribute) {
                    attributes.push(format!("route:{method} {path}"));
                }
                attributes.push("framework:web-route".to_string());
            }
        }
        _ => {}
    }
    if target.contains("fastapi") || target.contains("APIRouter") {
        attributes.push("framework:fastapi".to_string());
    }
    if target.contains("flask") || target.contains("Blueprint") {
        attributes.push("framework:flask".to_string());
    }
    attributes
}

fn python_test_attributes(relative_path: &str, kind: SymbolKind, name: &str) -> Vec<String> {
    let file_name = relative_path.rsplit('/').next().unwrap_or(relative_path);
    let is_test_file = file_name.starts_with("test_") || file_name.ends_with("_test.py");
    match kind {
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Test
            if is_test_file || name.starts_with("test_") =>
        {
            vec!["python:test".to_string(), "pytest:test".to_string()]
        }
        SymbolKind::Class if is_test_file || name.starts_with("Test") => {
            vec![
                "python:test-class".to_string(),
                "pytest:test-class".to_string(),
            ]
        }
        _ => Vec::new(),
    }
}

fn first_python_string_literal(text: &str) -> Option<String> {
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let quote = match ch {
            '\'' | '"' => ch,
            _ => continue,
        };
        let mut value = String::new();
        let mut escaped = false;
        for ch in chars.by_ref() {
            if escaped {
                value.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                return Some(value);
            } else {
                value.push(ch);
            }
        }
    }
    None
}

fn python_docs_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let Some(body) = node.child_by_field_name("body") else {
        return Vec::new();
    };
    let mut cursor = body.walk();
    let Some(first) = body.named_children(&mut cursor).next() else {
        return Vec::new();
    };
    let doc_node = if first.kind() == "expression_statement" {
        let mut first_cursor = first.walk();
        first
            .named_children(&mut first_cursor)
            .find(|child| child.kind() == "string")
    } else if first.kind() == "string" {
        Some(first)
    } else {
        None
    };
    doc_node
        .and_then(|node| node_text(node, source).ok())
        .map(|text| vec![text.trim().to_string()])
        .unwrap_or_default()
}

/// Returns true when the node lies inside a Python `@decorator(...)` head,
/// stopping at the enclosing function/class/lambda body.
fn python_node_is_inside_decorator(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "decorator" => return true,
            "function_definition" | "class_definition" | "lambda" => return false,
            _ => current = parent.parent(),
        }
    }
    false
}

fn python_class_bases(signature: &str) -> Vec<String> {
    let Some(after_class) = signature.trim().strip_prefix("class ") else {
        return Vec::new();
    };
    let Some(open_index) = after_class.find('(') else {
        return Vec::new();
    };
    let Some(close_index) = matching_close_paren(after_class, open_index) else {
        return Vec::new();
    };
    split_top_level_commas(&after_class[open_index + 1..close_index])
        .into_iter()
        // Class headers admit keyword arguments (`metaclass=`, `total=`,
        // `frozen=`, ...); those are not base classes and `python_type_name_from_annotation`
        // would otherwise strip `metaclass=Meta` down to the keyword name
        // `"metaclass"` and silently drop `Meta`.
        .filter(|item| !item.contains('='))
        .filter_map(|base| python_type_name_from_annotation(&base))
        .collect()
}

fn python_type_annotations(signature: &str) -> Vec<String> {
    let mut annotations = Vec::new();
    if let Some(open_index) = signature.find('(')
        && let Some(close_index) = matching_close_paren(signature, open_index)
    {
        for parameter in split_top_level_commas(&signature[open_index + 1..close_index]) {
            if let Some((_, annotation)) = parameter.split_once(':')
                && let Some(name) = python_type_name_from_annotation(annotation)
            {
                annotations.push(name);
            }
        }
        let rest = &signature[close_index + 1..];
        if let Some((_, return_annotation)) = rest.split_once("->") {
            let return_annotation = return_annotation
                .split_once(':')
                .map(|(before, _)| before)
                .unwrap_or(return_annotation);
            if let Some(name) = python_type_name_from_annotation(return_annotation) {
                annotations.push(name);
            }
        }
    }
    annotations.sort();
    annotations.dedup();
    annotations
}

fn python_type_name_from_annotation(annotation: &str) -> Option<String> {
    let mut text = annotation
        .split('=')
        .next()
        .unwrap_or(annotation)
        .trim()
        .trim_matches(|ch: char| {
            matches!(
                ch,
                '\'' | '"' | '[' | ']' | '(' | ')' | '{' | '}' | ':' | ',' | ' '
            )
        })
        .trim_start_matches('*')
        .trim();
    if text.is_empty() {
        return None;
    }
    for separator in ['|', '[', ','] {
        if let Some((before, _)) = text.split_once(separator) {
            text = before.trim();
        }
    }
    if text.is_empty()
        || matches!(
            text,
            "None" | "Any" | "object" | "str" | "int" | "float" | "bool"
        )
    {
        return None;
    }
    Some(last_path_segment(text))
}

fn matching_close_paren(text: &str, open_index: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, ch) in text
        .char_indices()
        .skip_while(|(index, _)| *index < open_index)
    {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_commas(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    for (index, ch) in text.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                let value = text[start..index].trim();
                if !value.is_empty() {
                    values.push(value.to_string());
                }
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    let value = text[start..].trim();
    if !value.is_empty() {
        values.push(value.to_string());
    }
    values
}

fn parent_symbol_is_impl_or_trait(parent_symbol: &Option<SymbolId>) -> bool {
    parent_symbol
        .as_ref()
        .map(|id| id.0.contains("::impl:") || id.0.contains("::trait:"))
        .unwrap_or(false)
}

fn function_has_self_parameter(node: Node<'_>, source: &str) -> bool {
    let Some(parameters) = node.child_by_field_name("parameters") else {
        return false;
    };
    let Ok(text) = node_text(parameters, source) else {
        return false;
    };
    let first = text
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .split(',')
        .next()
        .unwrap_or_default()
        .trim();
    let first = first.trim_start_matches("mut ").trim();

    first == "self"
        || first.starts_with("self:")
        || first.starts_with("&self")
        || first.starts_with("&mut self")
        || (first.starts_with('&') && first.contains(" self"))
        || first.starts_with("mut self:")
}

fn symbol_name(node: Node<'_>, kind: SymbolKind, source: &str) -> Option<String> {
    if kind == SymbolKind::Impl {
        return Some(impl_name(node, source));
    }

    node.child_by_field_name("name")
        .and_then(|child| node_text(child, source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn impl_name(node: Node<'_>, source: &str) -> String {
    let raw = signature_text(node, node.child_by_field_name("body"), source);
    trim_impl_header(&raw.split_whitespace().collect::<Vec<_>>().join(" "))
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

fn symbol_id(
    file: &FileRecord,
    parent_id: Option<&SymbolId>,
    kind: SymbolKind,
    name: &str,
    span: SourceSpan,
) -> SymbolId {
    let kind_name = format!("{kind:?}").to_lowercase();
    let safe_name = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let base = parent_id
        .map(|id| id.0.clone())
        .unwrap_or_else(|| file.relative_path.clone());
    SymbolId::new(format!(
        "{base}::{kind_name}:{safe_name}@{}",
        span.start_byte
    ))
}

fn signature_text(node: Node<'_>, body: Option<Node<'_>>, source: &str) -> String {
    let start = node.start_byte();
    let end = body
        .map(|body| body.start_byte())
        .unwrap_or_else(|| node.end_byte());
    source
        .get(start..end)
        .unwrap_or_default()
        .trim()
        .trim_end_matches('=')
        .trim()
        .to_string()
}

fn visibility_text(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == "visibility_modifier")
        .and_then(|child| node_text(child, source).ok())
        .map(|text| text.trim().to_string())
}

fn attributes_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let mut attributes = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if (child.kind() == "attribute_item" || child.kind() == "inner_attribute_item")
            && let Ok(text) = node_text(child, source)
        {
            attributes.push(text.trim().to_string());
        }
    }
    attributes
}

fn docs_from_attributes(attributes: &[String]) -> Vec<String> {
    attributes
        .iter()
        .filter(|attr| attribute_path(attr).as_deref() == Some("doc"))
        .cloned()
        .collect()
}

fn is_test_function(attributes: &[String]) -> bool {
    attributes.iter().any(|attr| {
        attribute_path(attr)
            .and_then(|path| path.rsplit("::").next().map(str::to_string))
            .as_deref()
            == Some("test")
    })
}

/// Extract the attribute path (the identifier or `::`-separated path that
/// precedes any `(`, `=`, `]`, or `!`). Returns `None` for empty/unrecognized
/// inputs. The attribute text is expected to look like `#[<path>(...)]`,
/// `#[<path> = "..."]`, `#[<path>]`, or the inner-attribute form `#![...]`.
fn attribute_path(attribute: &str) -> Option<String> {
    let trimmed = attribute.trim_start();
    let body = trimmed
        .strip_prefix("#![")
        .or_else(|| trimmed.strip_prefix("#["))?;
    let body = body.trim_start();
    let path: String = body
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == ':')
        .collect();
    if path.is_empty() {
        None
    } else {
        Some(path.trim_end_matches(':').to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportSpec {
    path: String,
    alias: Option<String>,
    is_glob: bool,
}

fn expand_use_declaration(raw: &str) -> Vec<ImportSpec> {
    let Some(tree) = strip_use_declaration(raw) else {
        return Vec::new();
    };
    expand_use_tree(tree)
        .into_iter()
        .filter(|import| !import.path.is_empty())
        .collect()
}

fn strip_use_declaration(raw: &str) -> Option<&str> {
    let mut text = raw.trim().trim_end_matches(';').trim();
    if let Some(rest) = text.strip_prefix("pub") {
        text = rest.trim_start();
        if text.starts_with('(')
            && let Some(close) = text.find(')')
        {
            text = text[close + 1..].trim_start();
        }
    }
    text.strip_prefix("use").map(str::trim)
}

fn expand_use_tree(tree: &str) -> Vec<ImportSpec> {
    let tree = tree.trim();
    if tree.is_empty() {
        return Vec::new();
    }
    if let Some((prefix, inner, suffix)) = split_top_level_braces(tree) {
        let prefix = prefix.trim_end_matches("::").trim();
        let suffix = suffix.trim_start_matches("::").trim();
        let mut imports = Vec::new();
        for item in split_top_level_use_commas(inner) {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let combined = join_use_segments(prefix, item, suffix);
            imports.extend(expand_use_tree(&combined));
        }
        return imports;
    }

    let (path, alias) = split_use_alias(tree);
    let path = path.trim().trim_end_matches(';').trim().to_string();
    if path.ends_with("::self") {
        return vec![ImportSpec {
            path: path.trim_end_matches("::self").to_string(),
            alias,
            is_glob: false,
        }];
    }
    vec![ImportSpec {
        is_glob: path.ends_with("::*"),
        path,
        alias,
    }]
}

fn split_use_alias(path: &str) -> (&str, Option<String>) {
    path.rsplit_once(" as ")
        .map(|(path, alias)| (path, Some(alias.trim().to_string())))
        .unwrap_or((path, None))
}

fn split_top_level_braces(text: &str) -> Option<(&str, &str, &str)> {
    let mut depth = 0usize;
    let mut start = None;
    for (index, ch) in text.char_indices() {
        match ch {
            '{' => {
                if depth == 0 {
                    start = Some(index);
                }
                depth += 1;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let start = start?;
                    return Some((&text[..start], &text[start + 1..index], &text[index + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_use_commas(text: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, ch) in text.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                items.push(&text[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    items.push(&text[start..]);
    items
}

fn join_use_segments(prefix: &str, item: &str, suffix: &str) -> String {
    let item = if item == "self" { "" } else { item };
    [prefix, item, suffix]
        .into_iter()
        .filter(|segment| !segment.trim().is_empty())
        .map(|segment| segment.trim().trim_matches(':'))
        .collect::<Vec<_>>()
        .join("::")
}

fn extract_import(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let is_reexport = raw.trim_start().starts_with("pub");
    for import in expand_use_declaration(raw) {
        ctx.imports.push(ParsedImport {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            is_glob: import.is_glob,
            is_reexport,
            path: import.path,
            alias: import.alias,
            span: span_from_node(node),
            provenance: Provenance::new("tree-sitter-rust", "use declaration"),
        });
    }
}

fn extract_python_import(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let raw = node_text(node, ctx.source).unwrap_or_default().trim();
    let imports = if let Some(rest) = raw.strip_prefix("from ") {
        python_from_imports(rest, &ctx.file.relative_path)
    } else if let Some(rest) = raw.strip_prefix("import ") {
        python_plain_imports(rest)
    } else {
        Vec::new()
    };

    for (path, alias, is_glob) in imports {
        ctx.imports.push(ParsedImport {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            path,
            alias,
            is_glob,
            is_reexport: ctx.file.relative_path.ends_with("__init__.py"),
            span: span_from_node(node),
            provenance: Provenance::new("tree-sitter-python", "import declaration"),
        });
    }
}

fn python_plain_imports(rest: &str) -> Vec<(String, Option<String>, bool)> {
    rest.split(',')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            let (path, alias) = split_python_alias(part);
            Some((path.to_string(), alias.map(str::to_string), false))
        })
        .collect()
}

fn python_from_imports(rest: &str, relative_path: &str) -> Vec<(String, Option<String>, bool)> {
    let Some((module, names)) = rest.split_once(" import ") else {
        return Vec::new();
    };
    let module = normalize_python_import_module(module.trim(), relative_path);
    names
        .split(',')
        .filter_map(|part| {
            let part = part.trim().trim_matches(['(', ')']);
            if part.is_empty() {
                return None;
            }
            let (name, alias) = split_python_alias(part);
            let is_glob = name == "*";
            let path = if is_glob {
                format!("{module}.*")
            } else {
                format!("{module}.{name}")
            };
            Some((path, alias.map(str::to_string), is_glob))
        })
        .collect()
}

fn split_python_alias(text: &str) -> (&str, Option<&str>) {
    text.split_once(" as ")
        .map(|(path, alias)| (path.trim(), Some(alias.trim())))
        .unwrap_or_else(|| (text.trim(), None))
}

fn normalize_python_import_module(module: &str, relative_path: &str) -> String {
    let leading_dots = module.chars().take_while(|ch| *ch == '.').count();
    if leading_dots == 0 {
        return module.to_string();
    }

    let suffix = module.trim_start_matches('.');
    let mut package = python_module_path_for_relative_file(relative_path);
    if !relative_path.ends_with("__init__.py") {
        package.pop();
    }
    for _ in 1..leading_dots {
        package.pop();
    }
    if !suffix.is_empty() {
        package.extend(suffix.split('.').filter(|segment| !segment.is_empty()));
    }
    package.join(".")
}

fn python_module_path_for_relative_file(relative_path: &str) -> Vec<&str> {
    relative_path
        .trim_end_matches(".py")
        .trim_end_matches("/__init__")
        .trim_start_matches("src/")
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != "__init__")
        .collect()
}

fn extract_python_field_symbol(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) {
    let Some((parent_id, SymbolKind::Class)) = parent_symbol else {
        return;
    };
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let Some((left, right)) = split_python_assignment_like(raw) else {
        return;
    };
    let Some(name) = python_field_name_from_left(left) else {
        return;
    };
    let span = span_from_node(node);
    let mut attributes = vec!["python:field".to_string()];
    if let Some(annotation) = left
        .split_once(':')
        .and_then(|(_, annotation)| python_field_type_name(annotation))
    {
        attributes.push(format!("type:{annotation}"));
        ctx.references.push(ParsedReference {
            file_id: ctx.file.id.clone(),
            owner_id: Some(parent_id.clone()),
            text: annotation,
            kind: ReferenceKind::Type,
            span,
            provenance: Provenance::new("tree-sitter-python", "field annotation reference"),
        });
    }
    attributes.extend(python_field_attributes(right));
    attributes.sort();
    attributes.dedup();

    ctx.symbols.push(ParsedSymbol {
        id: symbol_id(&ctx.file, Some(parent_id), SymbolKind::Field, &name, span),
        file_id: ctx.file.id.clone(),
        parent_id: Some(parent_id.clone()),
        name,
        kind: SymbolKind::Field,
        span,
        body_span: None,
        signature: raw.trim().to_string(),
        visibility: None,
        docs: Vec::new(),
        attributes,
        provenance: Provenance::new("tree-sitter-python", "class field assignment"),
        confidence: Confidence::Heuristic,
        freshness: Freshness::Fresh,
    });
}

fn python_field_type_name(annotation: &str) -> Option<String> {
    let text = annotation
        .split('=')
        .next()
        .unwrap_or(annotation)
        .trim()
        .trim_matches(|ch: char| {
            matches!(
                ch,
                '\'' | '"' | '[' | ']' | '(' | ')' | '{' | '}' | ':' | ',' | ' '
            )
        });
    if text.is_empty() {
        None
    } else {
        Some(last_path_segment(text))
    }
}

fn split_python_assignment_like(text: &str) -> Option<(&str, &str)> {
    if let Some((left, right)) = text.split_once('=') {
        return Some((left.trim(), right.trim()));
    }
    if let Some((left, annotation)) = text.split_once(':') {
        return Some((left.trim(), annotation.trim()));
    }
    None
}

fn python_field_name_from_left(left: &str) -> Option<String> {
    let name = left
        .split_once(':')
        .map(|(name, _)| name)
        .unwrap_or(left)
        .trim();
    python_simple_assignment_name(name)
}

fn python_field_attributes(right: &str) -> Vec<String> {
    let mut attributes = Vec::new();
    let callee = python_assignment_target(right).unwrap_or_else(|| right.trim().to_string());
    let lowered = callee.to_ascii_lowercase();
    if lowered.contains("column") || lowered.contains("mapped_column") {
        attributes.push("sqlalchemy:field".to_string());
    }
    if callee.contains("models.") && callee.contains("Field") {
        attributes.push("django:field".to_string());
    }
    if lowered.contains("field") {
        attributes.push("python:field-factory".to_string());
        attributes.push("dataclass:field".to_string());
        attributes.push("pydantic:field".to_string());
    }
    attributes
}

fn extract_python_assignment(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let Some((left, right)) = raw.split_once('=') else {
        return;
    };
    let left = left.trim();
    let right = right.trim();
    if left == "__all__" {
        for exported in python_string_list_values(right) {
            ctx.imports.push(ParsedImport {
                file_id: ctx.file.id.clone(),
                owner_id: owner_id.clone(),
                path: exported,
                alias: None,
                is_glob: false,
                is_reexport: true,
                span: span_from_node(node),
                provenance: Provenance::new("tree-sitter-python", "__all__ export"),
            });
        }
        return;
    }

    let Some(alias) = python_simple_assignment_name(left) else {
        return;
    };
    let Some(target) = python_assignment_target(right) else {
        return;
    };
    if alias == last_path_segment(&target) {
        return;
    }

    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id,
        path: target,
        alias: Some(alias),
        is_glob: false,
        is_reexport: false,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-python", "assignment alias"),
    });
}

fn python_simple_assignment_name(left: &str) -> Option<String> {
    let left = left.trim();
    if left.contains('.') || left.contains('[') || left.contains(',') {
        return None;
    }
    if is_python_identifier(left) {
        Some(left.to_string())
    } else {
        None
    }
}

fn python_assignment_target(right: &str) -> Option<String> {
    let expression = right
        .split_once('#')
        .map(|(before, _)| before)
        .unwrap_or(right)
        .trim();
    if expression.is_empty() {
        return None;
    }
    let callee = expression
        .find('(')
        .map(|index| expression[..index].trim())
        .unwrap_or(expression)
        .trim();
    let starts_with_literal = callee
        .chars()
        .next()
        .map(|ch| matches!(ch, '\'' | '"' | '[' | '{' | '(') || ch.is_ascii_digit())
        .unwrap_or(false);
    if callee.is_empty() || starts_with_literal {
        return None;
    }
    if callee
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.')
        && callee.split('.').all(is_python_identifier)
    {
        Some(callee.to_string())
    } else {
        None
    }
}

fn python_string_list_values(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        let quote = match ch {
            '\'' | '"' => ch,
            _ => continue,
        };
        let mut escaped = false;
        let mut value = String::new();
        for (_, ch) in chars.by_ref() {
            if escaped {
                value.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                if !value.is_empty() {
                    values.push(value);
                }
                break;
            } else {
                value.push(ch);
            }
        }
    }
    values
}

fn is_python_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn extract_direct_call(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let Some(function_node) = node.child_by_field_name("function") else {
        return;
    };
    let target_text = node_text(function_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if target_text.is_empty() {
        return;
    }
    let name = last_path_segment(&target_text);
    let arity = node
        .child_by_field_name("arguments")
        .map(|arguments| named_child_count(arguments))
        .unwrap_or_default();

    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: target_text.clone(),
        receiver: receiver_from_direct_call(&target_text),
        arity,
        kind: ParsedCallKind::Direct,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-rust", "call_expression"),
        confidence: Confidence::Heuristic,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

fn extract_python_call(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let Some(function_node) = node.child_by_field_name("function").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).next()
    }) else {
        return;
    };
    let target_text = node_text(function_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if target_text.is_empty() {
        return;
    }
    let name = last_path_segment(&target_text);
    let receiver = receiver_from_direct_call(&target_text);
    let arity = node
        .child_by_field_name("arguments")
        .or_else(|| {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() == "argument_list")
        })
        .map(|arguments| named_child_count(arguments))
        .unwrap_or_default();

    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: target_text.clone(),
        receiver,
        arity,
        kind: if target_text.contains('.') {
            ParsedCallKind::Method
        } else {
            ParsedCallKind::Direct
        },
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-python", "call"),
        confidence: Confidence::Heuristic,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

fn extract_method_call(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let name = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("method"))
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|| method_name_from_text(raw));
    if name.is_empty() {
        return;
    }
    let receiver = node
        .child_by_field_name("receiver")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .or_else(|| receiver_from_method_text(raw, &name));
    let arity = node
        .child_by_field_name("arguments")
        .map(|arguments| named_child_count(arguments))
        .unwrap_or_default();

    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: raw.trim().to_string(),
        receiver,
        arity,
        kind: ParsedCallKind::Method,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-rust", "method_call_expression"),
        confidence: Confidence::CandidateSet,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

fn extract_macro_call(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let target = raw.split('!').next().unwrap_or_default().trim().to_string();
    if target.is_empty() {
        return;
    }

    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name: last_path_segment(&target),
        target_text: raw.trim().to_string(),
        receiver: None,
        arity: 0,
        kind: ParsedCallKind::Macro,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-rust", "macro_invocation"),
        confidence: Confidence::MacroOpaque,
    });
    extract_body_hit(node, BodyHitKind::Macro, ctx, owner_id);
}

fn extract_reference(
    node: Node<'_>,
    kind: ReferenceKind,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() {
        return;
    }
    let body_kind = match kind {
        ReferenceKind::Identifier => BodyHitKind::Identifier,
        ReferenceKind::Type => BodyHitKind::Type,
        ReferenceKind::Path => BodyHitKind::Path,
        ReferenceKind::Field => BodyHitKind::Identifier,
        ReferenceKind::Attribute => BodyHitKind::Attribute,
    };

    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: text.clone(),
        kind,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-rust", format!("{} reference", node.kind())),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: body_kind,
        span: span_from_node(node),
    });
}

fn extract_python_reference(
    node: Node<'_>,
    kind: ReferenceKind,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() {
        return;
    }
    let body_kind = match kind {
        ReferenceKind::Identifier => BodyHitKind::Identifier,
        ReferenceKind::Field => BodyHitKind::Identifier,
        ReferenceKind::Attribute => BodyHitKind::Attribute,
        ReferenceKind::Type => BodyHitKind::Type,
        ReferenceKind::Path => BodyHitKind::Path,
    };

    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: text.clone(),
        kind,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-python", format!("{} reference", node.kind())),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: body_kind,
        span: span_from_node(node),
    });
}

fn extract_body_hit(
    node: Node<'_>,
    kind: BodyHitKind,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() {
        return;
    }
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind,
        span: span_from_node(node),
    });
}

fn reference_kind(kind: &str) -> Option<ReferenceKind> {
    match kind {
        "identifier" => Some(ReferenceKind::Identifier),
        "type_identifier" | "primitive_type" | "scoped_type_identifier" => {
            Some(ReferenceKind::Type)
        }
        "scoped_identifier" => Some(ReferenceKind::Path),
        "field_identifier" | "shorthand_field_identifier" => Some(ReferenceKind::Field),
        "attribute_item" => Some(ReferenceKind::Attribute),
        _ => None,
    }
}

fn is_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal"
            | "raw_string_literal"
            | "integer_literal"
            | "float_literal"
            | "boolean_literal"
            | "char_literal"
    )
}

fn is_python_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string" | "integer" | "float" | "true" | "false" | "none"
    )
}

fn receiver_from_direct_call(target_text: &str) -> Option<String> {
    target_text
        .rsplit_once("::")
        .or_else(|| target_text.rsplit_once('.'))
        .map(|(receiver, _)| receiver.trim().to_string())
        .filter(|receiver| !receiver.is_empty())
}

fn receiver_from_method_text(raw: &str, name: &str) -> Option<String> {
    let before_args = raw.split('(').next().unwrap_or_default();
    before_args
        .strip_suffix(name)
        .and_then(|prefix| prefix.strip_suffix('.'))
        .map(|receiver| receiver.trim().to_string())
        .filter(|receiver| !receiver.is_empty())
}

fn method_name_from_text(raw: &str) -> String {
    raw.split('(')
        .next()
        .and_then(|before_args| before_args.rsplit('.').next())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn last_path_segment(path: &str) -> String {
    path.rsplit("::")
        .next()
        .unwrap_or(path)
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .rsplit('.')
        .next()
        .unwrap_or(path)
        .trim()
        .trim_end_matches('!')
        .to_string()
}

fn named_child_count(node: Node<'_>) -> usize {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).count()
}

fn node_text<'source>(
    node: Node<'_>,
    source: &'source str,
) -> std::result::Result<&'source str, std::str::Utf8Error> {
    node.utf8_text(source.as_bytes())
}

fn span_from_node(node: Node<'_>) -> SourceSpan {
    SourceSpan::new(
        node.start_byte() as u32,
        node.end_byte() as u32,
        SourcePoint::new(
            node.start_position().row as u32,
            node.start_position().column as u32,
        ),
        SourcePoint::new(
            node.end_position().row as u32,
            node.end_position().column as u32,
        ),
    )
}

fn span_from_range(range: tree_sitter::Range) -> SourceSpan {
    SourceSpan::new(
        range.start_byte as u32,
        range.end_byte as u32,
        SourcePoint::new(
            range.start_point.row as u32,
            range.start_point.column as u32,
        ),
        SourcePoint::new(range.end_point.row as u32, range.end_point.column as u32),
    )
}

fn span_from_edit(edit: &InputEdit) -> SourceSpan {
    SourceSpan::new(
        edit.start_byte as u32,
        edit.new_end_byte as u32,
        SourcePoint::new(
            edit.start_position.row as u32,
            edit.start_position.column as u32,
        ),
        SourcePoint::new(
            edit.new_end_position.row as u32,
            edit.new_end_position.column as u32,
        ),
    )
}

fn input_edit(old: &str, new: &str) -> InputEdit {
    let old_bytes = old.as_bytes();
    let new_bytes = new.as_bytes();
    let prefix = common_prefix(old_bytes, new_bytes);
    let suffix = common_suffix(&old_bytes[prefix..], &new_bytes[prefix..]);
    let old_end_byte = old_bytes.len() - suffix;
    let new_end_byte = new_bytes.len() - suffix;

    InputEdit {
        start_byte: prefix,
        old_end_byte,
        new_end_byte,
        start_position: point_for_byte(old, prefix),
        old_end_position: point_for_byte(old, old_end_byte),
        new_end_position: point_for_byte(new, new_end_byte),
    }
}

fn common_prefix(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(left, right)| left == right)
        .count()
}

fn common_suffix(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .rev()
        .zip(right.iter().rev())
        .take_while(|(left, right)| left == right)
        .count()
}

fn point_for_byte(source: &str, byte: usize) -> Point {
    let mut row = 0;
    let mut column = 0;
    for current in source.as_bytes().iter().take(byte) {
        if *current == b'\n' {
            row += 1;
            column = 0;
        } else {
            column += 1;
        }
    }
    Point { row, column }
}

pub fn edge_kind_for_call(call: ParsedCallKind) -> EdgeKind {
    match call {
        ParsedCallKind::Direct | ParsedCallKind::Method => EdgeKind::Calls,
        ParsedCallKind::Macro => EdgeKind::InvokesMacro,
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
