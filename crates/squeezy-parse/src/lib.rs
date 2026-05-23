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
    pub is_static: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    csharp_parser: Parser,
    c_parser: Parser,
    cpp_parser: Parser,
    go_parser: Parser,
    javascript_parser: Parser,
    jsx_parser: Parser,
    rust_parser: Parser,
    java_parser: Parser,
    python_parser: Parser,
    typescript_parser: Parser,
    tsx_parser: Parser,
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
        let csharp_parser = parser_with_csharp_language()?;
        let c_parser = parser_with_c_language()?;
        let cpp_parser = parser_with_cpp_language()?;
        let go_parser = parser_with_go_language()?;
        let javascript_parser = parser_with_javascript_language()?;
        let jsx_parser = parser_with_jsx_language()?;
        let rust_parser = parser_with_rust_language()?;
        let java_parser = parser_with_java_language()?;
        let python_parser = parser_with_python_language()?;
        let typescript_parser = parser_with_typescript_language()?;
        let tsx_parser = parser_with_tsx_language()?;
        Ok(Self {
            csharp_parser,
            c_parser,
            cpp_parser,
            go_parser,
            javascript_parser,
            jsx_parser,
            rust_parser,
            java_parser,
            python_parser,
            typescript_parser,
            tsx_parser,
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
            LanguageKind::CSharp => Ok(&mut self.csharp_parser),
            LanguageKind::Cpp => Ok(&mut self.cpp_parser),
            LanguageKind::Go => Ok(&mut self.go_parser),
            LanguageKind::Java => Ok(&mut self.java_parser),
            LanguageKind::JavaScript => Ok(&mut self.javascript_parser),
            LanguageKind::Jsx => Ok(&mut self.jsx_parser),
            LanguageKind::Rust => Ok(&mut self.rust_parser),
            LanguageKind::Python => Ok(&mut self.python_parser),
            LanguageKind::TypeScript => Ok(&mut self.typescript_parser),
            LanguageKind::Tsx => Ok(&mut self.tsx_parser),
            _ => Err(SqueezyError::Parse(format!(
                "unsupported parser language {language:?}"
            ))),
        }
    }
}

fn parse_job_chunk(jobs: Vec<ParseJob>) -> Result<Vec<ParseOutput>> {
    let mut parsers = WorkerParsers {
        csharp: parser_with_csharp_language()?,
        c: parser_with_c_language()?,
        cpp: parser_with_cpp_language()?,
        go: parser_with_go_language()?,
        javascript: parser_with_javascript_language()?,
        jsx: parser_with_jsx_language()?,
        rust: parser_with_rust_language()?,
        java: parser_with_java_language()?,
        python: parser_with_python_language()?,
        typescript: parser_with_typescript_language()?,
        tsx: parser_with_tsx_language()?,
    };
    let mut outputs = Vec::with_capacity(jobs.len());
    for job in jobs {
        outputs.push(parse_record_with_cache(&mut parsers, job)?);
    }
    Ok(outputs)
}

struct WorkerParsers {
    csharp: Parser,
    c: Parser,
    cpp: Parser,
    go: Parser,
    javascript: Parser,
    jsx: Parser,
    rust: Parser,
    java: Parser,
    python: Parser,
    typescript: Parser,
    tsx: Parser,
}

impl WorkerParsers {
    fn parser_for_language(&mut self, language: LanguageKind) -> Result<&mut Parser> {
        match language {
            LanguageKind::C => Ok(&mut self.c),
            LanguageKind::CSharp => Ok(&mut self.csharp),
            LanguageKind::Cpp => Ok(&mut self.cpp),
            LanguageKind::Go => Ok(&mut self.go),
            LanguageKind::Java => Ok(&mut self.java),
            LanguageKind::JavaScript => Ok(&mut self.javascript),
            LanguageKind::Jsx => Ok(&mut self.jsx),
            LanguageKind::Rust => Ok(&mut self.rust),
            LanguageKind::Python => Ok(&mut self.python),
            LanguageKind::TypeScript => Ok(&mut self.typescript),
            LanguageKind::Tsx => Ok(&mut self.tsx),
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

fn parser_with_csharp_language() -> Result<Parser> {
    let mut parser = Parser::new();
    let language = csharp_language();
    parser
        .set_language(&language)
        .map_err(|err| SqueezyError::Parse(format!("failed to load C# grammar: {err}")))?;
    Ok(parser)
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

fn parser_with_javascript_language() -> Result<Parser> {
    let mut parser = Parser::new();
    let language = javascript_language();
    parser
        .set_language(&language)
        .map_err(|err| SqueezyError::Parse(format!("failed to load JavaScript grammar: {err}")))?;
    Ok(parser)
}

fn parser_with_jsx_language() -> Result<Parser> {
    let mut parser = Parser::new();
    let language = jsx_language();
    parser
        .set_language(&language)
        .map_err(|err| SqueezyError::Parse(format!("failed to load JSX grammar: {err}")))?;
    Ok(parser)
}

fn parser_with_typescript_language() -> Result<Parser> {
    let mut parser = Parser::new();
    let language = typescript_language();
    parser
        .set_language(&language)
        .map_err(|err| SqueezyError::Parse(format!("failed to load TypeScript grammar: {err}")))?;
    Ok(parser)
}

fn parser_with_tsx_language() -> Result<Parser> {
    let mut parser = Parser::new();
    let language = tsx_language();
    parser
        .set_language(&language)
        .map_err(|err| SqueezyError::Parse(format!("failed to load TSX grammar: {err}")))?;
    Ok(parser)
}

fn parser_with_java_language() -> Result<Parser> {
    let mut parser = Parser::new();
    let language = java_language();
    parser
        .set_language(&language)
        .map_err(|err| SqueezyError::Parse(format!("failed to load Java grammar: {err}")))?;
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

fn csharp_language() -> tree_sitter::Language {
    tree_sitter_c_sharp::LANGUAGE.into()
}

fn go_language() -> tree_sitter::Language {
    tree_sitter_go::LANGUAGE.into()
}

fn rust_language() -> tree_sitter::Language {
    tree_sitter_rust::LANGUAGE.into()
}

fn java_language() -> tree_sitter::Language {
    tree_sitter_java::LANGUAGE.into()
}

fn python_language() -> tree_sitter::Language {
    tree_sitter_python::LANGUAGE.into()
}

fn javascript_language() -> tree_sitter::Language {
    tree_sitter_javascript::LANGUAGE.into()
}

fn jsx_language() -> tree_sitter::Language {
    tree_sitter_javascript::LANGUAGE.into()
}

fn typescript_language() -> tree_sitter::Language {
    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
}

fn tsx_language() -> tree_sitter::Language {
    tree_sitter_typescript::LANGUAGE_TSX.into()
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
            | LanguageKind::CSharp
            | LanguageKind::Cpp
            | LanguageKind::Go
            | LanguageKind::Java
            | LanguageKind::JavaScript
            | LanguageKind::Jsx
            | LanguageKind::Rust
            | LanguageKind::Python
            | LanguageKind::TypeScript
            | LanguageKind::Tsx
    )
}

fn extract_language(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    match file.language {
        LanguageKind::C | LanguageKind::Cpp => extract_c_family(file, source, tree),
        LanguageKind::CSharp => extract_csharp(file, source, tree),
        LanguageKind::Go => extract_go(file, source, tree),
        LanguageKind::Java => extract_java(file, source, tree),
        LanguageKind::JavaScript
        | LanguageKind::Jsx
        | LanguageKind::TypeScript
        | LanguageKind::Tsx => extract_js_ts(file, source, tree),
        LanguageKind::Rust => extract_rust(file, source, tree),
        LanguageKind::Python => extract_python(file, source, tree),
        _ => ParsedFile::unsupported(
            file.clone(),
            format!("unsupported language for {}", file.relative_path),
        ),
    }
}

fn extract_csharp(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
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

    let mut scope = CsharpScope::default();
    visit_csharp_node(root, &mut ctx, None, None, &mut scope);
    dedup_csharp_facts(&mut ctx);

    ParsedFile {
        file,
        // Surface the file's dominant namespace as the `package` field, the
        // same way the Go extractor surfaces the file's `package` declaration.
        // File-scoped `namespace Foo;` and the first encountered braced
        // namespace both work; if neither is present this stays `None`.
        package: scope.top_namespace.clone(),
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

#[derive(Debug, Default, Clone)]
struct CsharpScope {
    namespace_segments: Vec<String>,
    top_namespace: Option<String>,
}

impl CsharpScope {
    fn current_namespace(&self) -> Option<String> {
        if self.namespace_segments.is_empty() {
            None
        } else {
            Some(self.namespace_segments.join("."))
        }
    }

    fn record_namespace(&mut self) {
        if self.top_namespace.is_some() {
            return;
        }
        if let Some(namespace) = self.current_namespace() {
            self.top_namespace = Some(namespace);
        }
    }
}

fn visit_csharp_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
    scope: &mut CsharpScope,
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
    match kind {
        "namespace_declaration" | "file_scoped_namespace_declaration" => {
            let raw_name = csharp_field_text(node, "name", ctx.source).unwrap_or_default();
            let segments = csharp_qualified_segments(&raw_name);
            let pushed = segments.len();
            scope.namespace_segments.extend(segments.clone());
            scope.record_namespace();
            if let Some(symbol) =
                csharp_namespace_symbol(node, ctx, &raw_name, parent_symbol.as_ref())
            {
                let next_parent = Some((symbol.id.clone(), symbol.kind));
                let next_owner = owner_symbol.clone();
                ctx.symbols.push(symbol);
                visit_csharp_children(node, ctx, next_parent, next_owner, scope);
            } else {
                visit_csharp_children(
                    node,
                    ctx,
                    parent_symbol.clone(),
                    owner_symbol.clone(),
                    scope,
                );
            }
            for _ in 0..pushed {
                scope.namespace_segments.pop();
            }
            return;
        }
        "using_directive" => {
            extract_csharp_using_directive(node, ctx, owner_symbol.clone());
        }
        _ => {}
    }

    if let Some(symbol) = csharp_symbol_from_node(node, ctx, parent_symbol.as_ref(), scope) {
        extract_csharp_symbol_facts(node, &symbol, ctx);
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        ctx.symbols.push(symbol);
        visit_csharp_children(node, ctx, next_parent, next_owner, scope);
        return;
    }

    match kind {
        "field_declaration" | "event_field_declaration" => {
            extract_csharp_field_symbols(node, ctx, parent_symbol.as_ref());
        }
        "invocation_expression" => {
            extract_csharp_call(node, ctx, owner_symbol.clone());
        }
        "object_creation_expression" => {
            extract_csharp_object_creation(node, ctx, owner_symbol.clone());
        }
        "identifier" if !is_csharp_declaration_name(node) => {
            extract_csharp_reference(node, ReferenceKind::Identifier, ctx, owner_symbol.clone());
        }
        "type_identifier" => {
            extract_csharp_reference(node, ReferenceKind::Type, ctx, owner_symbol.clone());
        }
        "generic_name" if !is_csharp_declaration_name(node) => {
            extract_csharp_reference(node, ReferenceKind::Type, ctx, owner_symbol.clone());
        }
        "qualified_name" => {
            extract_csharp_reference(node, ReferenceKind::Path, ctx, owner_symbol.clone());
        }
        kind if is_csharp_literal(kind) => {
            extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.clone());
        }
        _ => {}
    }

    visit_csharp_children(node, ctx, parent_symbol, owner_symbol, scope);
}

fn visit_csharp_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
    scope: &mut CsharpScope,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_csharp_node(
            child,
            ctx,
            parent_symbol.clone(),
            owner_symbol.clone(),
            scope,
        );
    }
}

fn csharp_namespace_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    raw_name: &str,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let trimmed = raw_name.trim();
    if trimmed.is_empty() {
        return None;
    }
    let span = span_from_node(node);
    let body = node.child_by_field_name("body");
    let signature = signature_text(node, body, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let id = symbol_id(
        &ctx.file,
        parent_id.as_ref(),
        SymbolKind::Module,
        trimmed,
        span,
    );
    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name: trimmed.to_string(),
        kind: SymbolKind::Module,
        span,
        body_span: body.map(span_from_node),
        signature,
        visibility: None,
        docs: Vec::new(),
        attributes: vec!["csharp:namespace".to_string()],
        provenance: Provenance::new(
            "tree-sitter-c-sharp",
            format!("{} declaration", node.kind()),
        ),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
    })
}

fn csharp_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    scope: &CsharpScope,
) -> Option<ParsedSymbol> {
    let kind = match node.kind() {
        "class_declaration" => SymbolKind::Class,
        // C# interfaces map to `SymbolKind::Interface` (added with the Go
        // semantic graph PR) so they sit beside Go interface declarations
        // rather than overloading Rust's `Trait` kind.
        "interface_declaration" => SymbolKind::Interface,
        "record_declaration" => SymbolKind::Struct,
        "struct_declaration" => SymbolKind::Struct,
        "enum_declaration" => SymbolKind::Enum,
        "delegate_declaration" => SymbolKind::TypeAlias,
        "method_declaration" | "local_function_statement" => SymbolKind::Method,
        "constructor_declaration" | "destructor_declaration" => SymbolKind::Method,
        "operator_declaration" | "conversion_operator_declaration" => SymbolKind::Method,
        "property_declaration" | "indexer_declaration" => SymbolKind::Field,
        "event_declaration" => SymbolKind::Field,
        "enum_member_declaration" => SymbolKind::Variant,
        _ => return None,
    };

    let mut kind = kind;
    if matches!(
        node.kind(),
        "method_declaration" | "local_function_statement"
    ) {
        let inside_type = parent_symbol
            .map(|(_, parent_kind)| {
                matches!(
                    parent_kind,
                    SymbolKind::Class
                        | SymbolKind::Struct
                        | SymbolKind::Trait
                        | SymbolKind::Interface
                        | SymbolKind::Enum
                )
            })
            .unwrap_or(false);
        if !inside_type {
            kind = SymbolKind::Function;
        }
    }

    let name = csharp_symbol_name(node, ctx.source)?;
    if name.is_empty() {
        return None;
    }

    let attributes_raw = csharp_attribute_strings(node, ctx.source);
    let modifiers = csharp_modifiers(node, ctx.source);
    let mut attributes = csharp_semantic_attributes(node, &attributes_raw, &modifiers);
    if matches!(node.kind(), "method_declaration") && csharp_is_test(&attributes_raw) {
        kind = SymbolKind::Test;
        attributes.push("csharp:test".to_string());
    }
    if matches!(node.kind(), "method_declaration")
        && csharp_is_test_filename(&ctx.file.relative_path)
        && !attributes.iter().any(|attr| attr == "csharp:test")
    {
        attributes.push("csharp:test-host".to_string());
    }
    if let Some(namespace) = scope.current_namespace() {
        attributes.push(format!("csharp:namespace:{namespace}"));
    }
    if matches!(
        node.kind(),
        "class_declaration" | "interface_declaration" | "record_declaration" | "struct_declaration"
    ) {
        for base in csharp_collect_base_types(node, ctx.source) {
            attributes.push(format!("base:{base}"));
        }
    }
    let docs = csharp_doc_comments(node, ctx.source);
    attributes.sort();
    attributes.dedup();

    let span = span_from_node(node);
    let body = node
        .child_by_field_name("body")
        .or_else(|| node.child_by_field_name("accessors"));
    let signature = signature_text(node, body, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let visibility = csharp_visibility(&modifiers);
    let id = symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span);

    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind,
        span,
        body_span: body.map(span_from_node),
        signature,
        visibility,
        docs,
        attributes,
        provenance: Provenance::new(
            "tree-sitter-c-sharp",
            format!("{} declaration", node.kind()),
        ),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
    })
}

fn csharp_symbol_name(node: Node<'_>, source: &str) -> Option<String> {
    if let Some(name_node) = node.child_by_field_name("name") {
        return node_text(name_node, source)
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
    }
    // operator_declaration uses an "operator" field; treat the operator token as the name.
    if let Some(op_node) = node.child_by_field_name("operator") {
        return node_text(op_node, source)
            .ok()
            .map(|text| format!("operator{}", text.trim()));
    }
    None
}

fn csharp_field_text(node: Node<'_>, field: &str, source: &str) -> Option<String> {
    let child = node.child_by_field_name(field)?;
    node_text(child, source)
        .ok()
        .map(|text| text.trim().to_string())
}

fn csharp_qualified_segments(raw: &str) -> Vec<String> {
    raw.split('.')
        .map(|segment| segment.trim().to_string())
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn csharp_attribute_strings(node: Node<'_>, source: &str) -> Vec<String> {
    let mut attributes = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "attribute_list" {
            let mut inner = child.walk();
            for attribute_node in child.named_children(&mut inner) {
                if attribute_node.kind() == "attribute"
                    && let Ok(text) = node_text(attribute_node, source)
                {
                    attributes.push(text.trim().to_string());
                }
            }
        }
    }
    attributes
}

fn csharp_modifiers(node: Node<'_>, source: &str) -> Vec<String> {
    let mut modifiers = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifier"
            && let Ok(text) = node_text(child, source)
        {
            modifiers.push(text.trim().to_string());
        }
    }
    modifiers
}

fn csharp_visibility(modifiers: &[String]) -> Option<String> {
    for visibility in ["public", "internal", "protected", "private", "file"] {
        if modifiers.iter().any(|modifier| modifier == visibility) {
            return Some(visibility.to_string());
        }
    }
    None
}

fn csharp_semantic_attributes(
    node: Node<'_>,
    attributes_raw: &[String],
    modifiers: &[String],
) -> Vec<String> {
    let mut attributes = Vec::new();
    for modifier in modifiers {
        attributes.push(format!("csharp:modifier:{modifier}"));
        if modifier == "partial" {
            attributes.push("csharp:partial".to_string());
        }
        if modifier == "static" {
            attributes.push("csharp:static".to_string());
        }
        if modifier == "abstract" {
            attributes.push("csharp:abstract".to_string());
        }
        if modifier == "async" {
            attributes.push("csharp:async".to_string());
        }
    }
    for attribute in attributes_raw {
        let cleaned = csharp_attribute_head(attribute);
        if cleaned.is_empty() {
            continue;
        }
        attributes.push(format!("csharp:attr:{cleaned}"));
        match cleaned.as_str() {
            "ApiController" | "Controller" => {
                attributes.push("framework:aspnet".to_string());
                attributes.push("framework:web-route".to_string());
            }
            "Route" => {
                attributes.push("framework:aspnet".to_string());
                attributes.push("framework:web-route".to_string());
                if let Some(path) = first_csharp_string_literal(attribute) {
                    attributes.push(format!("route:{path}"));
                }
            }
            "HttpGet" | "HttpPost" | "HttpPut" | "HttpPatch" | "HttpDelete" | "HttpOptions"
            | "HttpHead" => {
                let method = cleaned.trim_start_matches("Http").to_ascii_uppercase();
                attributes.push("framework:aspnet".to_string());
                attributes.push("framework:web-route".to_string());
                attributes.push(format!("route:{method}"));
                if let Some(path) = first_csharp_string_literal(attribute) {
                    attributes.push(format!("route:{method} {path}"));
                }
            }
            "Inject" => attributes.push("framework:di".to_string()),
            "Serializable" | "DataContract" => attributes.push("csharp:serializable".to_string()),
            _ => {}
        }
    }
    if matches!(
        node.kind(),
        "class_declaration" | "struct_declaration" | "record_declaration"
    ) {
        let _ = node;
    }
    attributes
}

fn csharp_attribute_head(attribute: &str) -> String {
    let body = attribute
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    let body = body
        .split_once(':')
        .map(|(_, rest)| rest.trim())
        .unwrap_or(body);
    let head = body.split('(').next().unwrap_or(body).trim();
    let head = head.rsplit('.').next().unwrap_or(head).trim();
    head.to_string()
}

fn csharp_is_test(attributes_raw: &[String]) -> bool {
    attributes_raw.iter().any(|attribute| {
        let head = csharp_attribute_head(attribute);
        matches!(
            head.as_str(),
            "Fact"
                | "Test"
                | "Theory"
                | "TestMethod"
                | "TestCase"
                | "TestCaseSource"
                | "InlineData"
                | "DataTestMethod"
                | "Property"
                | "FsCheck"
        )
    })
}

fn csharp_is_test_filename(relative_path: &str) -> bool {
    let file_name = relative_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(relative_path);
    let stem = file_name
        .strip_suffix(".cs")
        .or_else(|| file_name.strip_suffix(".csx"))
        .unwrap_or(file_name);
    let lower = stem.to_ascii_lowercase();
    lower.ends_with("tests") || lower.ends_with("test") || lower.contains(".tests.")
}

fn csharp_doc_comments(node: Node<'_>, source: &str) -> Vec<String> {
    let mut docs = Vec::new();
    let mut walker = node;
    while let Some(previous) = walker.prev_named_sibling() {
        walker = previous;
        match previous.kind() {
            "comment" => {
                if let Ok(text) = node_text(previous, source) {
                    let trimmed = text.trim();
                    if trimmed.starts_with("///") {
                        docs.push(trimmed.to_string());
                        continue;
                    }
                }
                break;
            }
            "attribute_list" => continue,
            _ => break,
        }
    }
    docs.reverse();
    docs
}

fn first_csharp_string_literal(text: &str) -> Option<String> {
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let quote = match ch {
            '"' => '"',
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

fn extract_csharp_using_directive(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let trimmed = raw.trim().trim_end_matches(';').trim();
    let body = trimmed.strip_prefix("using").unwrap_or(trimmed).trim();
    let is_global = body
        .strip_prefix("global")
        .map(|rest| {
            rest.trim_start()
                .starts_with(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        })
        .unwrap_or(false);
    let body = body.strip_prefix("global").unwrap_or(body).trim();
    let (is_static, body) = if let Some(rest) = body.strip_prefix("static") {
        (true, rest.trim())
    } else {
        (false, body)
    };
    let (alias, path) = match body.split_once('=') {
        Some((alias, target)) => (Some(alias.trim().to_string()), target.trim().to_string()),
        None => (None, body.trim().to_string()),
    };
    let path = path.trim().trim_end_matches(';').trim().to_string();
    if path.is_empty() {
        return;
    }
    let mut import = ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        path,
        alias,
        is_glob: is_static,
        is_reexport: is_global,
        is_static,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-c-sharp", "using directive"),
    };
    if is_static {
        import.path = format!("{}.*", import.path);
    }
    ctx.imports.push(import);
}

fn extract_csharp_call(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
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
    let (name, receiver, kind) = csharp_call_target_parts(function_node, &target_text, ctx.source);
    let arity = node
        .child_by_field_name("arguments")
        .map(|arguments| named_child_count(arguments))
        .unwrap_or_default();

    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: target_text.clone(),
        receiver,
        arity,
        kind,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-c-sharp", "invocation_expression"),
        confidence: Confidence::Heuristic,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

fn csharp_call_target_parts(
    function_node: Node<'_>,
    target_text: &str,
    source: &str,
) -> (String, Option<String>, ParsedCallKind) {
    match function_node.kind() {
        "member_access_expression" => {
            let name = function_node
                .child_by_field_name("name")
                .and_then(|name| node_text(name, source).ok())
                .map(|text| text.trim().to_string())
                .unwrap_or_else(|| last_path_segment(target_text));
            let receiver = function_node
                .child_by_field_name("expression")
                .and_then(|receiver| node_text(receiver, source).ok())
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty());
            (name, receiver, ParsedCallKind::Method)
        }
        "qualified_name" => (
            last_path_segment(target_text),
            receiver_from_direct_call(target_text),
            ParsedCallKind::Direct,
        ),
        "generic_name" => {
            let base = function_node
                .child_by_field_name("name")
                .and_then(|name| node_text(name, source).ok())
                .map(|text| text.trim().to_string())
                .unwrap_or_else(|| last_path_segment(target_text));
            (base, None, ParsedCallKind::Direct)
        }
        "alias_qualified_name" => (
            last_path_segment(target_text),
            receiver_from_direct_call(target_text),
            ParsedCallKind::Direct,
        ),
        "conditional_access_expression" | "element_access_expression" => {
            (last_path_segment(target_text), None, ParsedCallKind::Method)
        }
        _ => (
            last_path_segment(target_text),
            receiver_from_direct_call(target_text),
            ParsedCallKind::Direct,
        ),
    }
}

fn extract_csharp_object_creation(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let Some(type_node) = node.child_by_field_name("type") else {
        return;
    };
    let target_text = node_text(type_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if target_text.is_empty() {
        return;
    }
    let name = last_path_segment(&target_text);
    if name.is_empty() {
        return;
    }
    let arity = node
        .child_by_field_name("arguments")
        .map(|arguments| named_child_count(arguments))
        .unwrap_or_default();

    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name: name.clone(),
        target_text: target_text.clone(),
        receiver: receiver_from_direct_call(&target_text),
        arity,
        kind: ParsedCallKind::Direct,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-c-sharp", "object_creation_expression"),
        confidence: Confidence::Heuristic,
    });
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: target_text,
        kind: ReferenceKind::Type,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-c-sharp", "object_creation_expression"),
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

fn extract_csharp_reference(
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
    if csharp_is_keyword_or_predefined(&text) {
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
        provenance: Provenance::new("tree-sitter-c-sharp", format!("{} reference", node.kind())),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: body_kind,
        span: span_from_node(node),
    });
}

fn extract_csharp_field_symbols(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) {
    let Some((parent_id, parent_kind)) = parent_symbol else {
        return;
    };
    if !matches!(
        parent_kind,
        SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Trait
            | SymbolKind::Interface
            | SymbolKind::Enum
    ) {
        return;
    }
    let attributes_raw = csharp_attribute_strings(node, ctx.source);
    let modifiers = csharp_modifiers(node, ctx.source);
    let mut base_attributes = csharp_semantic_attributes(node, &attributes_raw, &modifiers);
    base_attributes.push("csharp:field".to_string());
    if node.kind() == "event_field_declaration" {
        base_attributes.push("csharp:event".to_string());
    }
    let mut cursor = node.walk();
    let mut declarations = Vec::new();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "variable_declaration" {
            declarations.push(child);
        }
    }
    for declaration in declarations {
        let mut declarator_cursor = declaration.walk();
        let type_node = declaration.child_by_field_name("type");
        let type_text = type_node
            .and_then(|node| node_text(node, ctx.source).ok())
            .map(|text| text.trim().to_string());
        for declarator in declaration.named_children(&mut declarator_cursor) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            let Some(name_node) = declarator.child_by_field_name("name") else {
                continue;
            };
            let Some(name) = node_text(name_node, ctx.source)
                .ok()
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
            else {
                continue;
            };
            let span = span_from_node(declarator);
            let mut attributes = base_attributes.clone();
            if let Some(type_text) = type_text.clone() {
                attributes.push(format!("type:{}", last_path_segment(&type_text)));
            }
            attributes.sort();
            attributes.dedup();
            let signature = signature_text(
                declaration,
                declarator.child_by_field_name("value"),
                ctx.source,
            );
            ctx.symbols.push(ParsedSymbol {
                id: symbol_id(&ctx.file, Some(parent_id), SymbolKind::Field, &name, span),
                file_id: ctx.file.id.clone(),
                parent_id: Some(parent_id.clone()),
                name,
                kind: SymbolKind::Field,
                span,
                body_span: None,
                signature,
                visibility: csharp_visibility(&modifiers),
                docs: Vec::new(),
                attributes,
                provenance: Provenance::new("tree-sitter-c-sharp", "field declaration"),
                confidence: Confidence::ExactSyntax,
                freshness: Freshness::Fresh,
            });
            if let Some(type_text) = type_text.clone() {
                ctx.references.push(ParsedReference {
                    file_id: ctx.file.id.clone(),
                    owner_id: Some(parent_id.clone()),
                    text: type_text,
                    kind: ReferenceKind::Type,
                    span,
                    provenance: Provenance::new("tree-sitter-c-sharp", "field type reference"),
                });
            }
        }
    }
}

fn csharp_collect_base_types(node: Node<'_>, source: &str) -> Vec<String> {
    let mut bases = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "base_list" {
            continue;
        }
        let mut base_cursor = child.walk();
        for base in child.named_children(&mut base_cursor) {
            let raw = match base.kind() {
                "primary_constructor_base_type" => base
                    .child_by_field_name("type")
                    .and_then(|type_node| node_text(type_node, source).ok()),
                _ => node_text(base, source).ok(),
            };
            if let Some(text) = raw
                && let Some(name) = csharp_type_name_from_annotation(text)
            {
                bases.push(name);
            }
        }
    }
    bases.sort();
    bases.dedup();
    bases
}

fn extract_csharp_symbol_facts(
    node: Node<'_>,
    symbol: &ParsedSymbol,
    ctx: &mut ExtractContext<'_>,
) {
    if matches!(
        node.kind(),
        "class_declaration" | "interface_declaration" | "record_declaration" | "struct_declaration"
    ) {
        for base in csharp_collect_base_types(node, ctx.source) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: base,
                kind: ReferenceKind::Type,
                span: symbol.span,
                provenance: Provenance::new("tree-sitter-c-sharp", "base type reference"),
            });
        }
    }
    if matches!(
        node.kind(),
        "method_declaration" | "local_function_statement" | "constructor_declaration"
    ) {
        if let Some(parameters) = node.child_by_field_name("parameters") {
            let mut cursor = parameters.walk();
            for parameter in parameters.named_children(&mut cursor) {
                if parameter.kind() != "parameter" {
                    continue;
                }
                if let Some(type_node) = parameter.child_by_field_name("type") {
                    push_csharp_type_reference(type_node, symbol, ctx, "parameter type reference");
                }
            }
        }
        if let Some(returns) = node.child_by_field_name("returns") {
            push_csharp_type_reference(returns, symbol, ctx, "return type reference");
        }
    }
}

fn push_csharp_type_reference(
    type_node: Node<'_>,
    symbol: &ParsedSymbol,
    ctx: &mut ExtractContext<'_>,
    reason: &'static str,
) {
    let Ok(text) = node_text(type_node, ctx.source) else {
        return;
    };
    let cleaned = csharp_type_name_from_annotation(text);
    let Some(cleaned) = cleaned else {
        return;
    };
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: Some(symbol.id.clone()),
        text: cleaned,
        kind: ReferenceKind::Type,
        span: symbol.span,
        provenance: Provenance::new("tree-sitter-c-sharp", reason),
    });
}

fn csharp_type_name_from_annotation(annotation: &str) -> Option<String> {
    let mut text = annotation
        .trim()
        .trim_matches(|ch: char| matches!(ch, '?' | '*' | '&' | ' '))
        .to_string();
    if let Some(open) = text.find('<') {
        text.truncate(open);
    }
    let stripped = text.trim().to_string();
    if stripped.is_empty() {
        return None;
    }
    let leaf = last_path_segment(&stripped);
    if csharp_is_keyword_or_predefined(&leaf) {
        return None;
    }
    Some(leaf)
}

fn is_csharp_declaration_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if let Some(name_node) = parent.child_by_field_name("name")
        && name_node.id() == node.id()
    {
        return true;
    }
    matches!(
        parent.kind(),
        "variable_declarator"
            | "type_parameter"
            | "parameter"
            | "method_declaration"
            | "class_declaration"
            | "interface_declaration"
            | "record_declaration"
            | "struct_declaration"
            | "enum_declaration"
            | "enum_member_declaration"
            | "namespace_declaration"
            | "file_scoped_namespace_declaration"
            | "property_declaration"
            | "field_declaration"
            | "event_declaration"
            | "event_field_declaration"
            | "delegate_declaration"
            | "constructor_declaration"
            | "destructor_declaration"
            | "local_function_statement"
    ) && parent
        .child_by_field_name("name")
        .map(|name_node| name_node.id() == node.id())
        .unwrap_or(false)
}

fn is_csharp_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal"
            | "verbatim_string_literal"
            | "raw_string_literal"
            | "integer_literal"
            | "real_literal"
            | "boolean_literal"
            | "character_literal"
            | "null_literal"
    )
}

fn csharp_is_keyword_or_predefined(text: &str) -> bool {
    matches!(
        text,
        "var"
            | "void"
            | "string"
            | "bool"
            | "byte"
            | "sbyte"
            | "char"
            | "decimal"
            | "double"
            | "float"
            | "int"
            | "uint"
            | "long"
            | "ulong"
            | "short"
            | "ushort"
            | "object"
            | "dynamic"
            | "nint"
            | "nuint"
            | "true"
            | "false"
            | "null"
            | "this"
            | "base"
            | "value"
    )
}

fn dedup_csharp_facts(ctx: &mut ExtractContext<'_>) {
    let mut import_seen = HashSet::new();
    ctx.imports.retain(|import| {
        import_seen.insert(format!(
            "{}|{:?}|{}|{:?}|{}|{}",
            import.file_id.0,
            import.owner_id.as_ref().map(|id| id.0.as_str()),
            import.path,
            import.alias,
            import.is_glob,
            import.is_reexport
        ))
    });

    let mut reference_seen = HashSet::new();
    ctx.references.retain(|reference| {
        reference_seen.insert(format!(
            "{}|{:?}|{}|{:?}|{}|{}",
            reference.file_id.0,
            reference.owner_id.as_ref().map(|id| id.0.as_str()),
            reference.text,
            reference.kind,
            reference.span.start_byte,
            reference.span.end_byte
        ))
    });
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

fn extract_java(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
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

    visit_java_node(root, &mut ctx, None, None);
    dedup_java_facts(&mut ctx);

    let package = ctx
        .imports
        .iter()
        .find(|import| import.alias.as_deref() == Some("__java_package__"))
        .map(|import| import.path.clone());

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

fn extract_js_ts(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
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

    visit_js_ts_node(root, &mut ctx, None, None);
    extract_js_ts_commonjs_facts(&mut ctx);
    dedup_js_ts_facts(&mut ctx);

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
            is_static: false,
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

fn visit_java_node(
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

    match node.kind() {
        "package_declaration" => extract_java_package(node, ctx),
        "import_declaration" => extract_java_import(node, ctx, owner_symbol.clone()),
        _ => {}
    }

    if node.kind() == "field_declaration" {
        let symbols = java_field_symbols_from_node(node, ctx, parent_symbol.as_ref());
        if !symbols.is_empty() {
            for symbol in symbols {
                ctx.symbols.push(symbol);
            }
            visit_java_children(node, ctx, parent_symbol, owner_symbol);
            return;
        }
    }

    if let Some(symbol) = java_symbol_from_node(node, ctx, parent_symbol.as_ref()) {
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        ctx.symbols.push(symbol);
        visit_java_children(node, ctx, next_parent, next_owner);
        return;
    }

    match node.kind() {
        "method_invocation" => {
            extract_java_method_invocation(node, ctx, owner_symbol.clone());
            visit_java_children(node, ctx, parent_symbol, owner_symbol);
        }
        "object_creation_expression" => {
            extract_java_object_creation(node, ctx, owner_symbol.clone());
            visit_java_children(node, ctx, parent_symbol, owner_symbol);
        }
        "identifier" => {}
        "type_identifier" | "scoped_type_identifier" => {
            extract_java_reference(node, ReferenceKind::Type, ctx, owner_symbol.clone())
        }
        "scoped_identifier" => {
            extract_java_reference(node, ReferenceKind::Path, ctx, owner_symbol.clone())
        }
        "field_access" => {
            extract_java_reference(node, ReferenceKind::Field, ctx, owner_symbol.clone())
        }
        "marker_annotation" | "annotation" => {
            extract_java_annotation_reference(node, ctx, owner_symbol)
        }
        kind if is_java_literal(kind) => {
            extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol)
        }
        _ => visit_java_children(node, ctx, parent_symbol, owner_symbol),
    }
}

fn visit_java_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_java_node(child, ctx, parent_symbol.clone(), owner_symbol.clone());
    }
}

fn java_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let kind = match node.kind() {
        "class_declaration" => SymbolKind::Class,
        "interface_declaration" | "annotation_type_declaration" => SymbolKind::Trait,
        "enum_declaration" => SymbolKind::Enum,
        "record_declaration" => SymbolKind::Struct,
        "annotation_type_element_declaration" => SymbolKind::Method,
        "method_declaration" => SymbolKind::Method,
        "constructor_declaration" => SymbolKind::Method,
        _ => return None,
    };

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
    let mut attributes = java_attributes_for_node(node, ctx.source);
    if is_java_test_symbol(&ctx.file.relative_path, kind, &name, &attributes) {
        attributes.push("java:test".to_string());
    }
    if matches!(
        kind,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait
    ) {
        attributes.extend(
            java_type_inheritance_names(node, ctx.source)
                .into_iter()
                .map(|base| format!("base:{base}")),
        );
    }
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
        visibility: java_visibility_text(node, ctx.source),
        docs: java_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-java", format!("{} declaration", node.kind())),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
    })
}

fn java_field_symbols_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Vec<ParsedSymbol> {
    let mut attributes = java_attributes_for_node(node, ctx.source);
    if let Some(field_type) = java_field_type(node, ctx.source) {
        attributes.push(format!("type:{field_type}"));
    }
    attributes.sort();
    attributes.dedup();

    let visibility = java_visibility_text(node, ctx.source);
    let docs = java_docs_for_node(node, ctx.source);
    let signature = signature_text(node, None, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());

    let mut symbols = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let Some(name) = child
            .child_by_field_name("name")
            .and_then(|grandchild| node_text(grandchild, ctx.source).ok())
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        let span = span_from_node(child);
        let id = symbol_id(
            &ctx.file,
            parent_id.as_ref(),
            SymbolKind::Field,
            &name,
            span,
        );
        symbols.push(ParsedSymbol {
            id,
            file_id: ctx.file.id.clone(),
            parent_id: parent_id.clone(),
            name,
            kind: SymbolKind::Field,
            span,
            body_span: None,
            signature: signature.clone(),
            visibility: visibility.clone(),
            docs: docs.clone(),
            attributes: attributes.clone(),
            provenance: Provenance::new("tree-sitter-java", "field_declaration declaration"),
            confidence: Confidence::ExactSyntax,
            freshness: Freshness::Fresh,
        });
    }
    symbols
}

fn extract_java_package(node: Node<'_>, ctx: &mut ExtractContext<'_>) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let Some(path) = raw
        .trim()
        .strip_prefix("package")
        .map(|text| text.trim().trim_end_matches(';').trim().to_string())
        .filter(|text| !text.is_empty())
    else {
        return;
    };
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id: None,
        path,
        alias: Some("__java_package__".to_string()),
        is_glob: false,
        is_reexport: true,
        is_static: false,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-java", "package declaration"),
    });
}

fn extract_java_import(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let Some(mut path) = raw
        .trim()
        .strip_prefix("import")
        .map(|text| text.trim().trim_end_matches(';').trim().to_string())
    else {
        return;
    };
    let is_static = path.strip_prefix("static ").is_some();
    if is_static {
        path = path.trim_start_matches("static ").trim().to_string();
    }
    if path.is_empty() {
        return;
    }
    let is_glob = path.ends_with(".*");
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id,
        path,
        alias: None,
        is_glob,
        is_reexport: false,
        is_static,
        span: span_from_node(node),
        provenance: Provenance::new(
            "tree-sitter-java",
            if is_static {
                "static import declaration"
            } else {
                "import declaration"
            },
        ),
    });
}

fn extract_java_method_invocation(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if raw.is_empty() {
        return;
    }
    let name = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| method_name_from_text(&raw));
    if name.is_empty() {
        return;
    }
    let receiver = node
        .child_by_field_name("object")
        .or_else(|| node.child_by_field_name("receiver"))
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .or_else(|| receiver_from_method_text(&raw, &name));
    let arity = node
        .child_by_field_name("arguments")
        .map(named_child_count)
        .unwrap_or_default();

    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: raw,
        receiver,
        arity,
        kind: ParsedCallKind::Method,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-java", "method_invocation"),
        confidence: Confidence::CandidateSet,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

fn extract_java_object_creation(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let target_text = node
        .child_by_field_name("type")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| {
            java_object_type_from_text(node_text(node, ctx.source).unwrap_or_default())
        });
    if target_text.is_empty() {
        return;
    }
    let arity = node
        .child_by_field_name("arguments")
        .map(named_child_count)
        .unwrap_or_default();
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name: last_path_segment(&target_text),
        target_text: target_text.clone(),
        receiver: receiver_from_direct_call(&target_text),
        arity,
        kind: ParsedCallKind::Direct,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-java", "object_creation_expression"),
        confidence: Confidence::Heuristic,
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text: target_text,
        kind: BodyHitKind::Call,
        span: span_from_node(node),
    });
}

fn extract_java_reference(
    node: Node<'_>,
    kind: ReferenceKind,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() || is_java_keyword(&text) {
        return;
    }
    let body_kind = match kind {
        ReferenceKind::Identifier | ReferenceKind::Field => None,
        ReferenceKind::Attribute => Some(BodyHitKind::Attribute),
        ReferenceKind::Type => Some(BodyHitKind::Type),
        ReferenceKind::Path => Some(BodyHitKind::Path),
    };
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: text.clone(),
        kind,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-java", format!("{} reference", node.kind())),
    });
    if let Some(body_kind) = body_kind {
        ctx.body_hits.push(BodyHit {
            file_id: ctx.file.id.clone(),
            owner_id,
            text,
            kind: body_kind,
            span: span_from_node(node),
        });
    }
}

fn extract_java_annotation_reference(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let name_node = node
        .child_by_field_name("name")
        .or_else(|| java_first_name_descendant(node));
    let text = name_node
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
        .unwrap_or_else(|| {
            let raw = node_text(node, ctx.source).unwrap_or_default();
            raw.trim()
                .trim_start_matches('@')
                .split('(')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string()
        });
    if text.is_empty() || is_java_keyword(&text) {
        return;
    }
    let span = name_node.map(span_from_node).unwrap_or_else(|| {
        let raw_span = span_from_node(node);
        SourceSpan::new(
            raw_span.start_byte.saturating_add(1).min(raw_span.end_byte),
            raw_span.end_byte,
            raw_span.start,
            raw_span.end,
        )
    });
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: text.clone(),
        kind: ReferenceKind::Attribute,
        span,
        provenance: Provenance::new("tree-sitter-java", format!("{} reference", node.kind())),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: BodyHitKind::Attribute,
        span,
    });
}

fn java_first_name_descendant(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "identifier" | "scoped_identifier" | "type_identifier" | "scoped_type_identifier"
        ) {
            return Some(child);
        }
        if let Some(found) = java_first_name_descendant(child) {
            return Some(found);
        }
    }
    None
}

fn dedup_java_facts(ctx: &mut ExtractContext<'_>) {
    let mut references: HashSet<(u32, ReferenceKind)> = HashSet::new();
    ctx.references
        .retain(|reference| references.insert((reference.span.start_byte, reference.kind)));
    let mut body_hits: HashSet<(u32, BodyHitKind)> = HashSet::new();
    ctx.body_hits
        .retain(|hit| body_hits.insert((hit.span.start_byte, hit.kind)));
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
                is_static: false,
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
        is_static: false,
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
        is_static: false,
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

fn visit_js_ts_node(
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
    if matches!(kind, "import_statement" | "export_statement") {
        extract_js_ts_import_export(node, ctx, owner_symbol.clone());
    }

    if let Some(symbol) = js_ts_synthetic_binding_symbol(node, ctx, parent_symbol.as_ref()) {
        ctx.symbols.push(symbol);
    }

    if let Some(symbol) = js_ts_symbol_from_node(node, ctx, parent_symbol.as_ref()) {
        extract_js_ts_symbol_facts(node, &symbol, ctx);
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        ctx.symbols.push(symbol);
        visit_js_ts_children(node, ctx, next_parent, next_owner);
        return;
    }

    if parent_symbol
        .as_ref()
        .map(|(_, parent_kind)| *parent_kind == SymbolKind::Class)
        .unwrap_or(false)
        && matches!(
            kind,
            "method_definition" | "public_field_definition" | "field_definition"
        )
    {
        visit_js_ts_children(node, ctx, None, owner_symbol);
        return;
    }

    if kind == "call_expression" || kind == "new_expression" {
        extract_js_ts_call(node, ctx, owner_symbol.clone());
    } else if let Some(reference_kind) = js_ts_reference_kind(kind) {
        extract_js_ts_reference(node, reference_kind, ctx, owner_symbol.clone());
    } else if is_js_ts_literal(kind) {
        extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.clone());
    }

    visit_js_ts_children(node, ctx, parent_symbol, owner_symbol);
}

fn visit_js_ts_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_js_ts_node(child, ctx, parent_symbol.clone(), owner_symbol.clone());
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
        && node.kind() != "variable_declarator"
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

fn js_ts_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let mut kind = match node.kind() {
        "class_declaration" => SymbolKind::Class,
        "enum_declaration" => SymbolKind::Enum,
        "function"
        | "function_declaration"
        | "function_expression"
        | "function_signature"
        | "generator_function"
        | "generator_function_declaration" => SymbolKind::Function,
        "interface_declaration" => SymbolKind::Interface,
        "ambient_declaration"
        | "internal_module"
        | "module"
        | "module_declaration"
        | "namespace_declaration" => SymbolKind::Module,
        "method_definition" | "method_signature" => SymbolKind::Method,
        "public_field_definition" | "field_definition" | "property_signature" => SymbolKind::Field,
        "type_alias_declaration" => SymbolKind::TypeAlias,
        "variable_declarator" => {
            if js_ts_variable_is_for_loop_local(node) {
                return None;
            }
            js_ts_variable_symbol_kind(node, ctx.source)?
        }
        _ => return None,
    };
    if kind == SymbolKind::Function
        && parent_symbol
            .map(|(_, parent_kind)| *parent_kind == SymbolKind::Class)
            .unwrap_or(false)
    {
        kind = SymbolKind::Method;
    }
    if kind == SymbolKind::Field
        && js_ts_node_value_is_function_like(node)
        && (parent_symbol
            .map(|(_, parent_kind)| *parent_kind == SymbolKind::Class)
            .unwrap_or(false)
            || node
                .parent()
                .map(|parent| matches!(parent.kind(), "class_body"))
                .unwrap_or(false))
    {
        kind = SymbolKind::Method;
    }

    let name = js_ts_symbol_name(node, kind, ctx.source)?;
    let body = js_ts_symbol_body(node, kind);
    let span = span_from_node(node);
    let body_span = body.map(span_from_node);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let id = symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span);
    let mut attributes = js_ts_attributes_for_symbol(node, kind, &name, ctx);
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
        signature: signature_text(node, body, ctx.source),
        visibility: js_ts_visibility_text(node, ctx.source),
        docs: js_ts_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-js-ts", format!("{} declaration", node.kind())),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
    })
}

fn js_ts_variable_symbol_kind(node: Node<'_>, source: &str) -> Option<SymbolKind> {
    let value = node.child_by_field_name("value");
    let value_kind = value.map(|node| node.kind()).unwrap_or_default();
    if matches!(
        value_kind,
        "arrow_function" | "function" | "function_expression" | "generator_function"
    ) {
        return Some(SymbolKind::Function);
    }
    if matches!(value_kind, "class" | "class_expression") {
        return Some(SymbolKind::Class);
    }
    let _ = source;
    Some(SymbolKind::Const)
}

/// A `variable_declarator` introduced by a C-style `for (let i = 0; ...; ...)`
/// is anchored on a `lexical_declaration` whose parent is the enclosing
/// `for_statement`. Loop counters and similar locals are excluded from the
/// declaration set in both Squeezy and the JS/TS oracle so navigation does
/// not get flooded by `i`/`j`/`len` per loop site.
fn js_ts_variable_is_for_loop_local(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !matches!(
        parent.kind(),
        "lexical_declaration" | "variable_declaration"
    ) {
        return false;
    }
    let Some(grand) = parent.parent() else {
        return false;
    };
    matches!(
        grand.kind(),
        "for_statement" | "for_in_statement" | "for_of_statement"
    )
}

fn js_ts_node_value_is_function_like(node: Node<'_>) -> bool {
    node.child_by_field_name("value")
        .map(|value| {
            matches!(
                value.kind(),
                "arrow_function" | "function" | "function_expression" | "generator_function"
            )
        })
        .unwrap_or(false)
}

fn js_ts_symbol_name(node: Node<'_>, kind: SymbolKind, source: &str) -> Option<String> {
    if kind == SymbolKind::Module {
        let raw_name = node
            .child_by_field_name("name")
            .and_then(|child| node_text(child, source).ok())?
            .trim()
            .to_string();
        if raw_name.starts_with(['"', '\'']) {
            return None;
        }
        return Some(js_ts_clean_property_name(&raw_name)).filter(|text| !text.is_empty());
    }
    if kind == SymbolKind::Method {
        if js_ts_method_is_accessor(node, source) {
            return None;
        }
        let name_node = node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("property"));
        if let Some(name) = name_node {
            let name = node_text(name, source)
                .ok()
                .map(js_ts_clean_property_name)
                .filter(|text| !text.is_empty())?;
            if name == "constructor" {
                return None;
            }
            return Some(name);
        }
    }
    if node.kind() == "variable_declarator" {
        return node
            .child_by_field_name("name")
            .and_then(|child| node_text(child, source).ok())
            .and_then(js_ts_binding_name);
    }
    // JavaScript field/method definitions expose the identifier as `property`,
    // while TypeScript uses `name`; falling back covers both grammars without
    // duplicating the rest of the lookup.
    node.child_by_field_name("name")
        .or_else(|| node.child_by_field_name("property"))
        .and_then(|child| node_text(child, source).ok())
        .map(js_ts_clean_property_name)
        .filter(|text| !text.is_empty())
}

fn js_ts_binding_name(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if is_js_ts_identifier(trimmed) {
        return Some(trimmed.to_string());
    }
    None
}

/// Tree-sitter parses a few constructs as flat keyword + identifier sequences
/// rather than `variable_declarator` or `module_declaration` wrappers, so the
/// regular declaration walk does not pick them up:
///
/// - `declare global { ... }` is an `ambient_declaration` whose `global`
///   segment is an anonymous keyword token rather than a named identifier
///   child. TypeScript treats it as a Module named `global`.
/// - `using x = expr` / `await using x = expr` (TC39 Stage 3) parse as an
///   `assignment_expression` whose first anonymous child is the `using`
///   keyword and whose `left` field is the binding identifier. TypeScript
///   treats these as ordinary `VariableDeclaration` Const symbols.
///
/// Synthesizing matching graph symbols keeps the JS/TS oracle from flagging
/// them as false negatives without forcing downstream consumers to special
/// case these surface syntaxes.
fn js_ts_synthetic_binding_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    match node.kind() {
        "ambient_declaration" => js_ts_declare_global_symbol(node, ctx, parent_symbol),
        "assignment_expression" => js_ts_using_binding_symbol(node, ctx, parent_symbol),
        _ => None,
    }
}

fn js_ts_declare_global_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let mut cursor = node.walk();
    let mut declare_seen = false;
    for child in node.children(&mut cursor) {
        if child.is_named() {
            return None;
        }
        let text = node_text(child, ctx.source).ok()?.trim().to_string();
        if !declare_seen {
            if text == "declare" {
                declare_seen = true;
            } else {
                return None;
            }
            continue;
        }
        if text == "global" {
            let span = span_from_node(child);
            let parent_id = parent_symbol.map(|(id, _)| id.clone());
            let id = symbol_id(
                &ctx.file,
                parent_id.as_ref(),
                SymbolKind::Module,
                "global",
                span,
            );
            let attributes = vec![
                js_ts_language_tag(ctx.file.language),
                "declare:global".to_string(),
            ]
            .into_iter()
            .filter(|attr| !attr.is_empty())
            .collect();
            let body_span = {
                let mut walker = node.walk();
                node.children(&mut walker)
                    .find(|child| child.kind() == "statement_block")
                    .map(span_from_node)
            };
            return Some(ParsedSymbol {
                id,
                file_id: ctx.file.id.clone(),
                parent_id,
                name: "global".to_string(),
                kind: SymbolKind::Module,
                span,
                body_span,
                signature: "declare global".to_string(),
                visibility: None,
                docs: Vec::new(),
                attributes,
                provenance: Provenance::new(
                    "tree-sitter-js-ts",
                    "declare global module".to_string(),
                ),
                confidence: Confidence::ExactSyntax,
                freshness: Freshness::Fresh,
            });
        }
        return None;
    }
    None
}

/// Recognize `using x = expr` and `await using x = expr` bindings. The
/// tree-sitter grammar (still pre-`using`) parses these as an assignment
/// expression with `using` as the first anonymous token; everything that
/// looks like a normal assignment is rejected so we never emit a Const for
/// `obj.foo = bar` or other reassignments.
fn js_ts_using_binding_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let mut cursor = node.walk();
    let mut first_anonymous_token: Option<String> = None;
    for child in node.children(&mut cursor) {
        if child.is_named() {
            break;
        }
        if let Ok(text) = node_text(child, ctx.source) {
            let token = text.trim();
            if !token.is_empty() {
                first_anonymous_token = Some(token.to_string());
                break;
            }
        }
    }
    if first_anonymous_token.as_deref() != Some("using") {
        return None;
    }
    let left = node.child_by_field_name("left")?;
    if left.kind() != "identifier" {
        return None;
    }
    let name = node_text(left, ctx.source).ok()?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let span = span_from_node(left);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let id = symbol_id(
        &ctx.file,
        parent_id.as_ref(),
        SymbolKind::Const,
        &name,
        span,
    );
    let attributes = vec![js_ts_language_tag(ctx.file.language), "using".to_string()]
        .into_iter()
        .filter(|attr| !attr.is_empty())
        .collect();
    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind: SymbolKind::Const,
        span,
        body_span: None,
        signature: node_text(node, ctx.source).unwrap_or_default().to_string(),
        visibility: None,
        docs: Vec::new(),
        attributes,
        provenance: Provenance::new("tree-sitter-js-ts", "using declaration".to_string()),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
    })
}

fn js_ts_language_tag(language: LanguageKind) -> String {
    match language {
        LanguageKind::JavaScript => "javascript".to_string(),
        LanguageKind::Jsx => "jsx".to_string(),
        LanguageKind::TypeScript => "typescript".to_string(),
        LanguageKind::Tsx => "tsx".to_string(),
        _ => String::new(),
    }
}

/// Detect `get foo()`/`set foo()` accessors on `method_definition` and
/// `method_signature` nodes. We look at the header (everything up to the
/// parameter list or method body) for a `get` or `set` keyword token that
/// is not the method's own name. Inspecting only the header avoids the old
/// substring scan that misfired on benign occurrences of " set " or " get "
/// inside comments or the method body itself.
fn js_ts_method_is_accessor(node: Node<'_>, source: &str) -> bool {
    let name_node = node.child_by_field_name("name");
    let header_end = node
        .child_by_field_name("parameters")
        .map(|params| params.start_byte())
        .or_else(|| {
            node.child_by_field_name("body")
                .map(|body| body.start_byte())
        })
        .unwrap_or(node.end_byte());
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.start_byte() >= header_end {
            break;
        }
        if matches!(name_node, Some(name) if name.id() == child.id()) {
            break;
        }
        if let Ok(text) = node_text(child, source) {
            let token = text.trim();
            if token == "get" || token == "set" {
                return true;
            }
        }
    }
    false
}

fn js_ts_clean_property_name(text: &str) -> String {
    let name = text.trim().trim_matches(['"', '\'']).to_string();
    if name.starts_with('#') || name.starts_with('[') || name.contains('[') || name.contains(']') {
        String::new()
    } else {
        name
    }
}

fn js_ts_symbol_body(node: Node<'_>, kind: SymbolKind) -> Option<Node<'_>> {
    node.child_by_field_name("body").or_else(|| {
        if matches!(kind, SymbolKind::Function | SymbolKind::Method) {
            node.child_by_field_name("value")
                .or_else(|| node.child_by_field_name("right"))
        } else {
            None
        }
    })
}

fn js_ts_attributes_for_symbol(
    node: Node<'_>,
    kind: SymbolKind,
    name: &str,
    ctx: &ExtractContext<'_>,
) -> Vec<String> {
    let mut attributes = Vec::new();
    match ctx.file.language {
        LanguageKind::JavaScript => attributes.push("javascript".to_string()),
        LanguageKind::Jsx => attributes.push("jsx".to_string()),
        LanguageKind::TypeScript => attributes.push("typescript".to_string()),
        LanguageKind::Tsx => attributes.push("tsx".to_string()),
        _ => {}
    }
    if matches!(ctx.file.language, LanguageKind::Jsx | LanguageKind::Tsx)
        && matches!(
            kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Class
        )
        && name
            .chars()
            .next()
            .map(|ch| ch.is_ascii_uppercase())
            .unwrap_or(false)
    {
        attributes.push("framework:component-like".to_string());
        attributes.push("jsx:component".to_string());
    }
    if js_ts_node_has_jsx_descendant(node) {
        attributes.push("jsx:returns-jsx".to_string());
    }
    attributes.extend(js_ts_decorator_attributes(node, ctx.source));
    attributes
}

fn js_ts_decorator_attributes(node: Node<'_>, source: &str) -> Vec<String> {
    let Ok(raw) = node_text(node, source) else {
        return Vec::new();
    };
    raw.lines()
        .map(str::trim)
        .take_while(|line| line.starts_with('@'))
        .filter_map(|line| {
            let name = line
                .trim_start_matches('@')
                .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '.'))
                .next()
                .unwrap_or_default();
            (!name.is_empty()).then(|| format!("decorator:{name}"))
        })
        .collect()
}

fn js_ts_node_has_jsx_descendant(node: Node<'_>) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind().starts_with("jsx_") {
            return true;
        }
        let mut cursor = current.walk();
        stack.extend(current.named_children(&mut cursor));
    }
    false
}

fn js_ts_visibility_text(node: Node<'_>, source: &str) -> Option<String> {
    let raw = node_text(node, source).ok()?.trim_start();
    ["public", "private", "protected", "readonly", "static"]
        .into_iter()
        .find(|keyword| raw.starts_with(*keyword))
        .map(str::to_string)
}

fn js_ts_docs_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let start = node.start_byte();
    let before = source.get(..start).unwrap_or_default();
    let Some(comment_start) = before.rfind("/**") else {
        return Vec::new();
    };
    let between = before[comment_start..].trim();
    if between.ends_with("*/") && between.lines().count() <= 20 {
        vec![between.to_string()]
    } else {
        Vec::new()
    }
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

fn extract_js_ts_symbol_facts(node: Node<'_>, symbol: &ParsedSymbol, ctx: &mut ExtractContext<'_>) {
    if matches!(symbol.kind, SymbolKind::Class | SymbolKind::Interface) {
        for type_name in js_ts_extends_implements_names(&symbol.signature) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: type_name,
                kind: ReferenceKind::Type,
                span: symbol.span,
                provenance: Provenance::new("tree-sitter-js-ts", "heritage reference"),
            });
        }
    }
    for type_name in js_ts_type_reference_names(&symbol.signature) {
        ctx.references.push(ParsedReference {
            file_id: ctx.file.id.clone(),
            owner_id: Some(symbol.id.clone()),
            text: type_name.clone(),
            kind: ReferenceKind::Type,
            span: symbol.span,
            provenance: Provenance::new("tree-sitter-js-ts", "type annotation reference"),
        });
        ctx.body_hits.push(BodyHit {
            file_id: ctx.file.id.clone(),
            owner_id: Some(symbol.id.clone()),
            text: type_name,
            kind: BodyHitKind::Type,
            span: symbol.span,
        });
    }
    let _ = node;
}

fn js_ts_extends_implements_names(signature: &str) -> Vec<String> {
    let mut names = Vec::new();
    for keyword in ["extends", "implements"] {
        let Some((_, rest)) = signature.split_once(keyword) else {
            continue;
        };
        let before_body = rest
            .split_once('{')
            .map(|(before, _)| before)
            .unwrap_or(rest)
            .split_once(" from ")
            .map(|(before, _)| before)
            .unwrap_or(rest);
        names.extend(
            split_top_level_commas(before_body)
                .into_iter()
                .filter_map(|name| js_ts_type_name_from_annotation(&name)),
        );
    }
    names.sort();
    names.dedup();
    names
}

fn js_ts_type_reference_names(signature: &str) -> Vec<String> {
    let mut names = Vec::new();
    for segment in signature.split([':', '<', '|', '&']) {
        if let Some(name) = js_ts_type_name_from_annotation(segment) {
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    names
}

fn js_ts_type_name_from_annotation(annotation: &str) -> Option<String> {
    let text = annotation
        .split(['=', ';', ',', ')', '(', '[', ']', '{', '}'])
        .next()
        .unwrap_or(annotation)
        .trim()
        .trim_start_matches("readonly ")
        .trim();
    if text.is_empty()
        || matches!(
            text,
            "any"
                | "bigint"
                | "boolean"
                | "false"
                | "never"
                | "null"
                | "number"
                | "object"
                | "string"
                | "symbol"
                | "true"
                | "undefined"
                | "unknown"
                | "void"
        )
    {
        return None;
    }
    let name = last_path_segment(text);
    if is_js_ts_identifier(&name)
        && name
            .chars()
            .next()
            .map(|ch| ch.is_ascii_uppercase())
            .unwrap_or(false)
    {
        Some(name)
    } else {
        None
    }
}

fn java_attributes_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let mut attributes = Vec::new();
    let modifiers = java_modifiers_node(node);
    let Some(modifiers) = modifiers else {
        return attributes;
    };
    let mut cursor = modifiers.walk();
    for child in modifiers.named_children(&mut cursor) {
        let Some(annotation) = java_annotation_name(child, source) else {
            continue;
        };
        attributes.push(format!("java:annotation:{annotation}"));
        let leaf = annotation.rsplit('.').next().unwrap_or(annotation.as_str());
        match leaf {
            "Test" | "ParameterizedTest" => attributes.push("junit:test".to_string()),
            "Override" => attributes.push("java:override".to_string()),
            _ => {}
        }
    }
    attributes
}

fn java_modifiers_node(node: Node<'_>) -> Option<Node<'_>> {
    if let Some(modifiers) = node.child_by_field_name("modifiers") {
        return Some(modifiers);
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == "modifiers")
}

fn java_annotation_name(node: Node<'_>, source: &str) -> Option<String> {
    if !matches!(node.kind(), "marker_annotation" | "annotation") {
        return None;
    }
    let name_node = node
        .child_by_field_name("name")
        .or_else(|| java_first_name_descendant(node))?;
    let raw = node_text(name_node, source).ok()?.trim().to_string();
    if raw.is_empty() { None } else { Some(raw) }
}

fn java_visibility_text(node: Node<'_>, source: &str) -> Option<String> {
    let modifiers = java_modifiers_node(node)?;
    let mut cursor = modifiers.walk();
    for child in modifiers.children(&mut cursor) {
        let raw = node_text(child, source).unwrap_or_default().trim();
        if matches!(raw, "public" | "protected" | "private") {
            return Some(raw.to_string());
        }
    }
    None
}

fn java_docs_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let mut docs = Vec::new();
    let Some(mut previous) = node.prev_named_sibling() else {
        return docs;
    };
    while previous.kind() == "line_comment" || previous.kind() == "block_comment" {
        if let Ok(text) = node_text(previous, source) {
            let trimmed = text
                .trim()
                .trim_start_matches("/**")
                .trim_start_matches("/*")
                .trim_start_matches("//")
                .trim_end_matches("*/")
                .trim()
                .to_string();
            if !trimmed.is_empty() {
                docs.push(trimmed);
            }
        }
        let Some(next_previous) = previous.prev_named_sibling() else {
            break;
        };
        previous = next_previous;
    }
    docs.reverse();
    docs
}

fn java_type_inheritance_names(node: Node<'_>, source: &str) -> Vec<String> {
    let mut names = Vec::new();
    for field in ["superclass", "interfaces"] {
        if let Some(child) = node.child_by_field_name(field) {
            collect_java_type_names(child, source, &mut names);
        }
    }
    names.sort();
    names.dedup();
    names
}

fn collect_java_type_names(node: Node<'_>, source: &str, names: &mut Vec<String>) {
    if matches!(
        node.kind(),
        "type_identifier" | "scoped_type_identifier" | "generic_type"
    ) && let Ok(text) = node_text(node, source)
        && let Some(name) = java_type_name_from_text(text)
    {
        names.push(name);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_java_type_names(child, source, names);
    }
}

fn java_type_name_from_text(text: &str) -> Option<String> {
    let clean = text
        .split('<')
        .next()
        .unwrap_or(text)
        .trim()
        .trim_end_matches("[]")
        .to_string();
    if clean.is_empty() || is_java_keyword(&clean) {
        None
    } else {
        Some(clean)
    }
}

fn java_field_type(node: Node<'_>, source: &str) -> Option<String> {
    if let Some(child) = node.child_by_field_name("type")
        && let Ok(text) = node_text(child, source)
        && let Some(name) = java_type_name_from_text(text)
    {
        return Some(name);
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| {
            matches!(
                child.kind(),
                "type_identifier"
                    | "scoped_type_identifier"
                    | "generic_type"
                    | "array_type"
                    | "integral_type"
                    | "floating_point_type"
                    | "boolean_type"
                    | "void_type"
            )
        })
        .and_then(|child| node_text(child, source).ok())
        .and_then(java_type_name_from_text)
}

fn java_object_type_from_text(raw: &str) -> String {
    raw.split_once("new ")
        .map(|(_, rest)| rest)
        .unwrap_or(raw)
        .split('(')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn is_java_test_symbol(
    relative_path: &str,
    kind: SymbolKind,
    name: &str,
    attributes: &[String],
) -> bool {
    matches!(kind, SymbolKind::Method | SymbolKind::Class)
        && (relative_path.contains("/test/")
            || relative_path.ends_with("Test.java")
            || name.ends_with("Test")
            || attributes.iter().any(|attribute| attribute == "junit:test"))
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
            is_static: false,
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
            is_static: false,
            span: span_from_node(node),
            provenance: Provenance::new("tree-sitter-python", "import declaration"),
        });
    }
}

fn extract_js_ts_import_export(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source).unwrap_or_default().trim();
    let is_reexport = raw.starts_with("export");
    let imports = js_ts_imports_from_statement(raw);
    for (path, alias, is_glob) in imports {
        ctx.imports.push(ParsedImport {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            path,
            alias,
            is_glob,
            is_reexport,
            is_static: false,
            span: span_from_node(node),
            provenance: Provenance::new("tree-sitter-js-ts", "import/export declaration"),
        });
    }
}

fn js_ts_imports_from_statement(raw: &str) -> Vec<(String, Option<String>, bool)> {
    let mut imports = Vec::new();
    if raw.starts_with("import") {
        let Some(module) = js_ts_module_specifier(raw) else {
            return imports;
        };
        let before_from = raw
            .split_once(" from ")
            .map(|(before, _)| before)
            .unwrap_or(raw)
            .trim()
            .trim_start_matches("import")
            .trim()
            .trim_end_matches(';')
            .trim();
        if before_from.is_empty() || before_from.starts_with(['"', '\'']) {
            imports.push((module, None, false));
            return imports;
        }
        if let Some(namespace) = before_from.strip_prefix("* as ") {
            imports.push((
                format!("{module}.*"),
                Some(namespace.trim().to_string()),
                true,
            ));
            return imports;
        }
        let (default_part, named_part) = split_js_ts_default_and_named_import(before_from);
        if let Some(default_name) = default_part.filter(|name| is_js_ts_identifier(name)) {
            imports.push((
                format!("{module}.default"),
                Some(default_name.to_string()),
                false,
            ));
        }
        if let Some(named) = named_part {
            for (imported, alias) in js_ts_named_imports(named) {
                imports.push((format!("{module}.{imported}"), alias, false));
            }
        }
    } else if raw.starts_with("export") {
        let Some(module) = js_ts_module_specifier(raw) else {
            for (exported, alias) in js_ts_named_imports(raw) {
                imports.push((exported, alias, false));
            }
            return imports;
        };
        if raw.contains("* from ") {
            imports.push((format!("{module}.*"), None, true));
        }
        for (exported, alias) in js_ts_named_imports(raw) {
            imports.push((format!("{module}.{exported}"), alias, false));
        }
    }
    imports
}

fn split_js_ts_default_and_named_import(text: &str) -> (Option<&str>, Option<&str>) {
    if let Some(open) = text.find('{') {
        let default = text[..open].trim().trim_end_matches(',').trim();
        let named = Some(&text[open..]);
        (Some(default).filter(|value| !value.is_empty()), named)
    } else {
        (Some(text.trim()).filter(|value| !value.is_empty()), None)
    }
}

fn js_ts_named_imports(text: &str) -> Vec<(String, Option<String>)> {
    let inside = if let Some(open) = text.find('{') {
        text[open + 1..]
            .split_once('}')
            .map(|(inside, _)| inside)
            .unwrap_or_default()
    } else {
        text.trim()
    };
    split_top_level_commas(inside)
        .into_iter()
        .filter_map(|part| {
            let part = part.trim().trim_start_matches("type ").trim();
            if part.is_empty() {
                return None;
            }
            let (imported, alias) = part
                .split_once(" as ")
                .map(|(left, right)| (left.trim(), Some(right.trim().to_string())))
                .unwrap_or((part, None));
            if is_js_ts_identifier(imported) {
                Some((imported.to_string(), alias))
            } else {
                None
            }
        })
        .collect()
}

fn js_ts_module_specifier(raw: &str) -> Option<String> {
    let source = if let Some((_, after_from)) = raw.rsplit_once(" from ") {
        after_from
    } else if raw.trim_start().starts_with("import") {
        raw.trim_start().trim_start_matches("import").trim()
    } else {
        return None;
    };
    first_js_ts_string_literal(source)
}

fn first_js_ts_string_literal(text: &str) -> Option<String> {
    let mut chars = text.char_indices();
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
                return Some(value);
            } else {
                value.push(ch);
            }
        }
    }
    None
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
                is_static: false,
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
        is_static: false,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-python", "assignment alias"),
    });
}

fn extract_js_ts_commonjs_facts(ctx: &mut ExtractContext<'_>) {
    for line in ctx.source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if let Some((left, right)) = line.split_once("require(") {
            let alias = js_ts_commonjs_alias(left);
            if let Some(module) = first_js_ts_string_literal(right)
                && let Some(alias) = alias
            {
                ctx.imports.push(ParsedImport {
                    file_id: ctx.file.id.clone(),
                    owner_id: None,
                    path: module,
                    alias: Some(alias),
                    is_glob: false,
                    is_reexport: false,
                    is_static: false,
                    span: SourceSpan::new(0, 0, SourcePoint::new(0, 0), SourcePoint::new(0, 0)),
                    provenance: Provenance::new("tree-sitter-js-ts", "commonjs require"),
                });
            }
        }
        if let Some((_, right)) = line.split_once("module.exports")
            && let Some((_, exported)) = right.split_once('=')
        {
            let exported = exported.trim().trim_end_matches(';').trim();
            if is_js_ts_identifier(exported) {
                ctx.imports.push(ParsedImport {
                    file_id: ctx.file.id.clone(),
                    owner_id: None,
                    path: exported.to_string(),
                    alias: None,
                    is_glob: false,
                    is_reexport: true,
                    is_static: false,
                    span: SourceSpan::new(0, 0, SourcePoint::new(0, 0), SourcePoint::new(0, 0)),
                    provenance: Provenance::new("tree-sitter-js-ts", "commonjs export"),
                });
            }
        }
    }
}

fn js_ts_commonjs_alias(left: &str) -> Option<String> {
    let left = left
        .trim()
        .trim_start_matches("const ")
        .trim_start_matches("let ")
        .trim_start_matches("var ")
        .trim();
    let alias = left.split('=').next()?.trim();
    if is_js_ts_identifier(alias) {
        Some(alias.to_string())
    } else {
        None
    }
}

fn dedup_js_ts_facts(ctx: &mut ExtractContext<'_>) {
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

fn extract_js_ts_call(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let function_node = node.child_by_field_name("function").or_else(|| {
        if node.kind() == "new_expression" {
            node.child_by_field_name("constructor")
        } else {
            None
        }
    });
    let Some(function_node) = function_node.or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).next()
    }) else {
        return;
    };
    let target_text = node_text(function_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .trim_start_matches("new ")
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
                .find(|child| child.kind() == "arguments")
        })
        .map(named_child_count)
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
        provenance: Provenance::new("tree-sitter-js-ts", node.kind()),
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

fn extract_js_ts_reference(
    node: Node<'_>,
    kind: ReferenceKind,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .trim_matches(['"', '\''])
        .to_string();
    if text.is_empty() || js_ts_reference_is_declaration_name(node) {
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
        provenance: Provenance::new("tree-sitter-js-ts", format!("{} reference", node.kind())),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: body_kind,
        span: span_from_node(node),
    });
}

fn js_ts_reference_is_declaration_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    matches!(
        parent.kind(),
        "class_declaration"
            | "enum_declaration"
            | "function_declaration"
            | "function"
            | "function_expression"
            | "function_signature"
            | "generator_function_declaration"
            | "generator_function"
            | "interface_declaration"
            | "method_definition"
            | "method_signature"
            | "public_field_definition"
            | "field_definition"
            | "property_signature"
            | "type_alias_declaration"
            | "variable_declarator"
    ) && parent
        .child_by_field_name("name")
        .map(|name| name == node)
        .unwrap_or(false)
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

fn js_ts_reference_kind(kind: &str) -> Option<ReferenceKind> {
    match kind {
        "identifier" | "shorthand_property_identifier" => Some(ReferenceKind::Identifier),
        "member_expression" | "nested_identifier" => Some(ReferenceKind::Path),
        "property_identifier" | "private_property_identifier" => Some(ReferenceKind::Field),
        "predefined_type" | "type_identifier" | "type_predicate" | "generic_type" => {
            Some(ReferenceKind::Type)
        }
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

fn is_js_ts_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string" | "template_string" | "number" | "true" | "false" | "null" | "undefined" | "regex"
    )
}

fn is_python_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string" | "integer" | "float" | "true" | "false" | "none"
    )
}

fn is_js_ts_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first == '$' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric())
}

fn is_java_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal"
            | "decimal_integer_literal"
            | "decimal_floating_point_literal"
            | "hex_integer_literal"
            | "true"
            | "false"
            | "null_literal"
            | "character_literal"
    )
}

fn is_java_keyword(text: &str) -> bool {
    matches!(
        text,
        "abstract"
            | "assert"
            | "boolean"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extends"
            | "final"
            | "finally"
            | "float"
            | "for"
            | "if"
            | "goto"
            | "implements"
            | "import"
            | "instanceof"
            | "int"
            | "interface"
            | "long"
            | "native"
            | "new"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "short"
            | "static"
            | "strictfp"
            | "super"
            | "switch"
            | "synchronized"
            | "this"
            | "throw"
            | "throws"
            | "transient"
            | "try"
            | "void"
            | "volatile"
            | "while"
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
