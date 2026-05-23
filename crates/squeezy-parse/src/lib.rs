use std::{collections::HashMap, fs};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

pub struct RustParser {
    rust_parser: Parser,
    python_parser: Parser,
    cache: HashMap<FileId, CachedParsedFile>,
}

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

impl RustParser {
    pub fn new() -> Result<Self> {
        let rust_parser = parser_with_rust_language()?;
        let python_parser = parser_with_python_language()?;
        Ok(Self {
            rust_parser,
            python_parser,
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
                            "parallel Rust parse worker panicked".to_string(),
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
        rust: parser_with_rust_language()?,
        python: parser_with_python_language()?,
    };
    let mut outputs = Vec::with_capacity(jobs.len());
    for job in jobs {
        outputs.push(parse_record_with_cache(&mut parsers, job)?);
    }
    Ok(outputs)
}

struct WorkerParsers {
    rust: Parser,
    python: Parser,
}

impl WorkerParsers {
    fn parser_for_language(&mut self, language: LanguageKind) -> Result<&mut Parser> {
        match language {
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

fn parser_with_python_language() -> Result<Parser> {
    let mut parser = Parser::new();
    let language = python_language();
    parser
        .set_language(&language)
        .map_err(|err| SqueezyError::Parse(format!("failed to load Python grammar: {err}")))?;
    Ok(parser)
}

fn rust_language() -> tree_sitter::Language {
    tree_sitter_rust::LANGUAGE.into()
}

fn python_language() -> tree_sitter::Language {
    tree_sitter_python::LANGUAGE.into()
}

fn is_supported_language(language: LanguageKind) -> bool {
    matches!(language, LanguageKind::Rust | LanguageKind::Python)
}

fn extract_language(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    match file.language {
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

    ParsedFile {
        file,
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

    if kind == "call" {
        extract_python_call(node, ctx, owner_symbol.clone());
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
    let attributes = python_attributes_for_node(node, ctx.source);

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
        docs: Vec::new(),
        attributes,
        provenance: Provenance::new("tree-sitter-python", format!("{} declaration", node.kind())),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
    })
}

fn python_attributes_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let Some(parent) = node.parent() else {
        return Vec::new();
    };
    if parent.kind() != "decorated_definition" {
        return Vec::new();
    }
    let mut cursor = parent.walk();
    parent
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "decorator")
        .filter_map(|child| node_text(child, source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .collect()
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
        .filter(|attr| attr.contains("doc"))
        .cloned()
        .collect()
}

fn is_test_function(attributes: &[String]) -> bool {
    attributes
        .iter()
        .any(|attr| attr.contains("#[test]") || attr.contains("::test"))
}

fn extract_import(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let path = raw
        .trim()
        .trim_start_matches("pub")
        .trim_start_matches("use")
        .trim()
        .trim_end_matches(';')
        .trim()
        .to_string();
    let alias = path
        .rsplit_once(" as ")
        .map(|(_, alias)| alias.trim().to_string());

    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id,
        is_glob: path.ends_with("::*") || path.contains("::*"),
        is_reexport: raw.trim_start().starts_with("pub"),
        path,
        alias,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-rust", "use declaration"),
    });
}

fn extract_python_import(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let raw = node_text(node, ctx.source).unwrap_or_default().trim();
    let imports = if let Some(rest) = raw.strip_prefix("from ") {
        python_from_imports(rest)
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
            is_reexport: false,
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

fn python_from_imports(rest: &str) -> Vec<(String, Option<String>, bool)> {
    let Some((module, names)) = rest.split_once(" import ") else {
        return Vec::new();
    };
    let module = module.trim();
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
