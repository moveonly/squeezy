# Semantic AST-Based Code Retrieval

## Motivation

A naïve coding agent answers "what does `verify_token` do?" by reading
the whole file. If `verify_token` lives at line 800 of a 1500-line
`auth/middleware.rs`, the agent burns roughly 7000 input tokens to
look at ~50 lines of actual code. Every follow-up question — "who
calls it?", "what's its signature?", "where is it referenced?" —
re-pays that price because the model can only ask for files, not for
shapes.

Tree-sitter parses every supported file into typed AST nodes, but the
cost saving lives in the semantic layer Squeezy builds on top. Each
declaration is split into a `signature` slice and a `body_span`, then
`squeezy-graph` cross-links symbols by call, reference, and container
hierarchy. The model retrieves through that semantic index —
`definition_search`, `symbol_context`, `read_slice`, `repo_map` — and
fetches the smallest slice that answers the question. The signature
alone is typically 50–150 tokens; the body comes on demand. Whole-file
reads remain available but are no longer the default unit of
retrieval, and the difference compounds across an agent loop.

## Mechanism

### Tree-sitter parsers

`squeezy-parse` owns one `tree_sitter::Parser` per supported grammar
(constructed eagerly at `crates/squeezy-parse/src/lib.rs:258-285`) and
dispatches per `LanguageKind`:

```rust
// crates/squeezy-parse/src/lib.rs:457-474
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
```

The supported set is: Rust, Python, Java, Kotlin, Scala, C#, Go, C, C++,
JavaScript, JSX, TypeScript, TSX, PHP, Ruby, Swift, and Dart. The per-language
extractors live in
`crates/squeezy-parse/src/languages/` — one module per family
(`c_family.rs`, `csharp.rs`, `dart.rs`, `go.rs`, `java.rs`, `js_ts.rs`,
`kotlin.rs`, `php.rs`, `python.rs`, `ruby.rs`, `rust.rs`, `scala.rs`,
`swift.rs`) — each exporting an `extract_*` entry point that walks the
tree-sitter tree and returns a `ParsedFile`.

Each extractor produces the same five products: `symbols`, `imports`,
`calls`, `references`, and `body_hits` (literals, identifiers, type
references seen inside bodies). The shape is in
`crates/squeezy-parse/src/lib.rs:29-41`:

```rust
// crates/squeezy-parse/src/lib.rs:29-41
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
```

Unsupported files round-trip through `ParsedFile::unsupported`, which
suggests `"bounded read/grep/list navigation"` as the fallback.

### Signature / body split

The two most important slice fields are `signature_span` and `body_span`. Every
symbol carries one `span` (covering the entire declaration), a signature string,
an optional `signature_span` for the declaration header, and an optional
`body_span` (covering just the body). The structure is at
`crates/squeezy-parse/src/lib.rs:64-88`:

```rust
// crates/squeezy-parse/src/lib.rs:64-88
pub struct ParsedSymbol {
    pub id: SymbolId,
    pub file_id: FileId,
    pub parent_id: Option<SymbolId>,
    pub name: String,
    pub kind: SymbolKind,
    pub language_identity: Option<String>,
    pub span: SourceSpan,
    pub body_span: Option<SourceSpan>,
    pub signature: String,
    pub visibility: Option<String>,
    pub docs: Vec<String>,
    pub attributes: Vec<String>,
    pub provenance: Provenance,
    pub confidence: Confidence,
    pub freshness: Freshness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arity: Option<u8>,
}
```

The Rust extractor populates `body_span` by asking tree-sitter for the
`body` field of the declaration node. `signature_text` then slices from
the symbol's start byte up to wherever the body begins (or the symbol's
end byte for declarations without bodies). Both pieces live in
`crates/squeezy-parse/src/languages/rust.rs`:

```rust
// crates/squeezy-parse/src/languages/rust.rs:138-143
let body = node.child_by_field_name("body");
let span = span_from_node(node);
let body_span = body.map(span_from_node);
let signature = signature_text(node, body, ctx.source);
let visibility = visibility_text(node, ctx.source);
let id = symbol_id(&ctx.file, parent_symbol.as_ref(), kind, &name, span);
```

```rust
// crates/squeezy-parse/src/languages/rust.rs:1505-1517
pub(crate) fn signature_text(node: Node<'_>, body: Option<Node<'_>>, source: &str) -> String {
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
```

So for a Rust function `pub fn verify_token(token: &str) -> Result<Claims> { ... }`,
the `signature` field is the literal text `pub fn verify_token(token: &str) -> Result<Claims>`
and `body_span` is the `{ ... }`. The signature is stored eagerly (it
goes into the graph's lexical indices); the body lives at known byte
offsets and is fetched on demand. The same pattern applies in
`python_symbol_from_node` (`rust.rs:174-246`) and
`js_ts_symbol_from_node` (`rust.rs:248-332`) — they all share
`signature_text` and the `body_span = body.map(span_from_node)` line.
For `impl` blocks the trait/type header is reused as the symbol name
via `impl_name` / `trim_impl_header` at lines 1430-1476, so an `impl
Display for Token` block becomes a navigable graph node.

### Code graph

The graph is the central index built by
`squeezy-graph::SemanticGraph`. After all `ParsedFile`s are merged
through `insert_parsed_file` (`crates/squeezy-graph/src/lib.rs:935-972`),
the graph holds a battery of cross-cutting indices defined at
`crates/squeezy-graph/src/lib.rs:358-401`:

```rust
// crates/squeezy-graph/src/lib.rs:358-401
symbols_by_name: HashMap<String, Vec<SymbolId>>,
symbol_signature_lower: HashMap<SymbolId, String>,
signature_trigram_index: HashMap<[u8; 3], Vec<SymbolId>>,
body_hit_text_lower: Vec<String>,
body_hit_trigram_index: HashMap<[u8; 3], Vec<usize>>,
body_hit_trigram_indexed: bool,
references_by_text: HashMap<String, Vec<usize>>,
children_by_parent: HashMap<SymbolId, Vec<SymbolId>>,
edges_by_from: HashMap<SymbolId, Vec<usize>>,
edges_by_to: HashMap<SymbolId, Vec<usize>>,
imports_by_file: HashMap<FileId, Vec<usize>>,
imports_by_alias_target: HashMap<String, Vec<usize>>,
wildcard_aliased_imports: Vec<usize>,
java_package_by_file: HashMap<FileId, Vec<String>>,
js_ts_resolver: JsTsResolver,
arity_index: HashMap<(FileId, String, u8), SymbolId>,
importers_by_file: HashMap<FileId, Vec<FileId>>,
resolver_slots: cross_file::ResolverSlots,
```

The retrieval-critical indices: `symbols_by_name` for exact-name
lookups (the seed of every `definition_search`);
`signature_trigram_index` for the fuzzy-match prefilter;
`body_hit_trigram_index` for pivoting from a body string back to its
enclosing symbol; `children_by_parent` as the backbone of `repo_map`;
and `edges_by_from`/`edges_by_to` for call-graph queries.

Calls and references are surfaced through three pure-graph helpers at
`crates/squeezy-graph/src/lib.rs:875-922` and `:811-841`:

```rust
// crates/squeezy-graph/src/lib.rs:875-893
pub fn callees(&self, caller: &SymbolId) -> Vec<CallEdgeHit> {
    self.edges_by_from
        .get(caller)
        .into_iter()
        .flatten()
        .filter_map(|edge_index| self.edge_hit(*edge_index))
        .filter(|hit| matches!(hit.edge.kind, EdgeKind::Calls | EdgeKind::InvokesMacro))
        .collect()
}

pub fn callers(&self, callee: &SymbolId) -> Vec<CallEdgeHit> {
    self.edges_by_to
        .get(callee)
        .into_iter()
        .flatten()
        .filter_map(|edge_index| self.edge_hit(*edge_index))
        .filter(|hit| matches!(hit.edge.kind, EdgeKind::Calls | EdgeKind::InvokesMacro))
        .collect()
}
```

`references_to_symbol` (`:811-841`) drives the non-call references —
type mentions, identifier uses, attribute paths — through
`reference_candidate_indexes_for_symbol` and then validates each
candidate with `reference_binding_confidence` so the tools layer can
report the binding quality alongside the hit. `call_chain` (lines
895-923) wraps `callees` into a BFS for "does A eventually reach B?"
queries.

### Ranking

The graph returns *candidate* symbols. The `squeezy-rank` crate orders
them. It uses a layered ladder for one-word identifier queries and BM25
for multi-word natural-language queries.

The identifier ladder lives in
`crates/squeezy-rank/src/symbol_rank.rs:22-57`:

```rust
// crates/squeezy-rank/src/symbol_rank.rs:21-57
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RankTier {
    Exact = 0,
    CaseInsensitive = 1,
    SignatureSubstring = 2,
    TokenBag = 3,
    Fuzzy = 4,
    NoMatch = 5,
}

pub fn rank_symbol(symbol: GraphSymbolView<'_>, query: &str) -> (RankTier, i32) {
    if symbol.name == query {
        return (RankTier::Exact, 0);
    }
    if symbol.name.eq_ignore_ascii_case(query) {
        return (RankTier::CaseInsensitive, 0);
    }
    if symbol.signature.contains(query) {
        return (RankTier::SignatureSubstring, 0);
    }
    if token_bag_match(symbol.name, query) {
        return (RankTier::TokenBag, 0);
    }
    if let Some(score) = fuzzy_score(symbol.name, query) {
        return (RankTier::Fuzzy, score);
    }
    (RankTier::NoMatch, i32::MAX)
}
```

Lower-numbered tiers always beat higher tiers — an exact name match
beats every fuzzy match no matter how close, and a signature substring
beats every camelCase/snake_case split. That ordering matters because
it means the agent's first hit on `verify_token` is the function named
`verify_token`, not some unrelated function that happens to mention
"verify token" in a comment.

The BM25 reranker handles multi-token queries like
`"reject expired token"` where the user types prose rather than an
identifier. It runs only after the graph's trigram prefilter has cut
the candidate set down, and only when the query has 2+ tokens. The
constants and corpus shape are at
`crates/squeezy-rank/src/bm25_rank.rs:11-49`:

```rust
// crates/squeezy-rank/src/bm25_rank.rs:11-23
const K1: f32 = 1.2;
const B: f32 = 0.75;

pub struct BM25Doc<'a> {
    pub signature: &'a str,
    pub docs: &'a str,
    pub attributes: &'a str,
}
```

The per-symbol corpus is `signature` + `docs` + `attributes` joined by
spaces — not the body. That keeps the rerank cheap and biased toward
declarations (which is what `definition_search` is for) instead of
dragging in implementation details. K1 = 1.2 and B = 0.75 are the
textbook BM25 defaults; the comment notes the implementation is
in-tree specifically to avoid the unmaintained `fxhash` advisory that
the upstream `bm25` crate pulls in.

### Incremental parse

A full reindex on every keystroke would be both wasteful and
distracting. Squeezy caches each file's `(hash, tree)` and lets
tree-sitter reuse subtrees that didn't change. The cache lives at
`crates/squeezy-parse/src/lib.rs:219-224`:

```rust
// crates/squeezy-parse/src/lib.rs:218-224
#[derive(Debug, Clone)]
struct CachedParsedFile {
    hash: ContentHash,
    language: LanguageKind,
    source: String,
    tree: Tree,
}
```

The reuse path is at `crates/squeezy-parse/src/lib.rs:403-444`:

```rust
// crates/squeezy-parse/src/lib.rs:404-441
let (tree, changed_ranges) = match old.filter(|cached| cached.language == record.language) {
    Some(mut cached) if cached.hash != record.hash => {
        let edit = input_edit(&cached.source, &source);
        cached.tree.edit(&edit);
        let parser = self.parser_for_language(record.language)?;
        let new_tree = parser.parse(&source, Some(&cached.tree)).ok_or_else(|| {
            SqueezyError::Parse(format!("tree-sitter returned no {:?} tree", record.language))
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
            SqueezyError::Parse(format!("tree-sitter returned no {:?} tree", record.language))
        })?;
        (tree, Vec::new())
    }
};
```

The three branches:

1. Hash unchanged → reuse the cached tree, skip parsing entirely. The
   `changed_ranges` vector is empty.
2. Hash changed → call `input_edit` to compute the tree-sitter
   `InputEdit` from the old and new sources, apply `tree.edit(&edit)` to
   mark the old tree dirty, then call `parser.parse(&source, Some(&cached.tree))`.
   Tree-sitter reuses every subtree outside the dirty region; only the
   changed portions are re-parsed. `tree.changed_ranges(&new_tree)`
   then yields the byte ranges that actually moved, which downstream
   `annotate_dirty_ranges` (`crates/squeezy-graph/src/lib.rs:843-857`)
   uses to mark only intersecting symbols dirty.
3. No cache entry → cold parse, full file.

If `changed_ranges` comes back empty after an edit (rare but possible
when the edit lies entirely inside a re-used subtree), it falls back to
the raw `InputEdit` range so callers still get a non-empty hint.

### Model-facing tools

The agent never touches the graph directly. It sees four
`crates/squeezy-tools/src/graph_tools.rs` entry points, dispatched at
`:2165-2222`:

- `repo_map` — hierarchy of files/modules/classes/functions truncated
  to `max_depth` (default 2, max 5) and `max_files` (default 50, max
  200). Returns hierarchy nodes plus per-node evidence packets. Lines
  2225-2266.
- `definition_search` — query by name (or `symbol_id`) and get
  tier-ranked declarations. Each packet's next-action steers the model
  to `read_slice {span_kind: "signature"}` (line 2375-2383).
- `symbol_context` — given a symbol, return its callers, callees,
  references, and Cargo diagnostics. The packet's next-action is
  `read_slice {symbol_id, span_kind: "body"}` (lines 1233-1300).
- `read_slice` — universal slice fetcher.

`ReadSliceArgs` at `crates/squeezy-tools/src/graph_tools.rs:108-152`
accepts either a `symbol_id` + `span_kind` (the graph-driven path) or
explicit byte/line bounds. The `span_kind` enum is just two variants:

```rust
// crates/squeezy-tools/src/graph_tools.rs:146-152
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReadSliceSpanKind {
    #[default]
    Signature,
    Body,
}
```

The span resolution lives in `read_slice_target`:

```rust
// crates/squeezy-tools/src/graph_tools.rs:1962-1980
if let Some(symbol_id) = args.symbol_id.as_deref() {
    let graph =
        graph.ok_or_else(|| "read_slice symbol_id requires an available graph".to_string())?;
    let symbol = graph
        .symbols
        .get(&SymbolId::new(symbol_id))
        .ok_or_else(|| format!("symbol_id not found: {symbol_id}"))?;
    let span = match args.span_kind.unwrap_or_default() {
        ReadSliceSpanKind::Signature => symbol.span,
        ReadSliceSpanKind::Body => symbol.body_span.unwrap_or(symbol.span),
    };
    return Ok((
        symbol.file_id.0.clone(),
        Some(span),
        "graph_symbol",
        symbol.confidence,
        vec![symbol.provenance.clone()],
    ));
}
```

Note the fallback at line 1971: if `span_kind: "body"` is asked for a
symbol that has no `body_span` (a `const`, a trait method declaration
without a default body), it returns the full `span` rather than
erroring. The model gets the declaration, not silence.

## Worked example

The user asks: "Where does the auth middleware reject expired tokens?"
Assume an `auth/middleware.rs` file containing a `verify_token`
function near line 800 of a 1500-line file with a 50-line body.

**Step 1 — locate.** The model issues
`definition_search { "query": "expired" }`. The graph hits
`symbols_by_name`, the signature trigram index, and (since the query
is two-token-ish) BM25 over `signature + docs + attributes`. It
returns a tier-ranked packet list — say five hits, top of which is
`verify_token` with a docstring mentioning "rejects expired tokens".
Each packet's `next_action` points to
`read_slice {symbol_id, span_kind: "signature"}`. Cost: roughly
**400 tokens**. A naïve grep-and-read across the repo for "expired"
would be **15,000–40,000 tokens**.

**Step 2 — read the shape.** Knowing the symbol ID, the model asks
`read_slice {symbol_id, span_kind: "signature"}`. `read_slice_target`
uses `signature_span` when available and falls back to the declaration span only
for symbols that do not carry a separate header span:
`pub async fn verify_token(token: &str, now: SystemTime) -> Result<Claims, AuthError>`.
Cost: roughly **50 tokens**. A naïve `Read auth/middleware.rs` is
**~7000 tokens**.

**Step 3 — callers and references.** `symbol_context {symbol_id}`
returns the symbol summary plus callers (`graph.callers`), callees
(`graph.callees`), references (`graph.references_to_symbol`), and any
Cargo diagnostics. Each entry is a one-line `(name, file, span, kind)`
summary (see `graph_tools.rs:1233-1300`). Cost for three callers and a
handful of references: **~300 tokens**. A naïve approach greps for
`verify_token(`, reads each calling file in full — **~20,000 tokens**.

**Step 4 — read the body.** `read_slice {symbol_id, span_kind: "body"}`
returns just the 50-line function body via `body_span`. Cost:
**~400 tokens**.

**Totals.** Graph-driven: ~1150 tokens. Naïve: ~42,000 tokens. A 35×
reduction for the same answer, with the bonus that the model never
sees code it doesn't need.

## Edge cases and limits

**Unsupported languages.** Anything outside the supported `LanguageFamily` set
— for example Zig, Haskell, Elixir, Lua, Bash, SQL, HTML, CSS, YAML, TOML, and
Markdown — produces a
`ParsedFile::unsupported` and gets `"bounded read/grep/list navigation"`
as the documented fallback. The graph contains a `File`
symbol for the file but no declarations; the agent can still read it,
just with no slice shortcut.

**Grammar imperfections.** When tree-sitter parses with errors, the
extractor still proceeds but pushes a `ParseDiagnostic` with
`Confidence::Partial` (`languages/rust.rs:19-25`). Downstream packets
inherit that confidence, so the model can see that a slice came from
a partially-parsed file. Missing nodes (`node.is_missing()`) are
skipped with a diagnostic at `languages/rust.rs:49-55`.

**Missing `body_span`.** Some declarations have no body — `const`
items, externally-declared functions in C headers, trait method
declarations without a default impl, `abstract` methods in Java. For
these, `body_span` is `None` and `read_slice {span_kind: "body"}`
falls back to the full declaration span via
`symbol.body_span.unwrap_or(symbol.span)` at `graph_tools.rs:1971`.

**Incremental parse fallback.** The hash-cache-then-edit path
(`lib.rs:404-423`) gracefully degrades: language changes drop the
cache and force a cold parse; empty `changed_ranges` falls back to the
raw `InputEdit` range at line 421 so `annotate_dirty_ranges` still has
a hint. `parser.parse(..., Some(&cached.tree))` returning `None` is
treated as a hard parse error but is effectively unreachable on
well-formed input.

**BM25 has no semantics.** The reranker is purely lexical — "refresh",
"renew", "rotate" are three unrelated tokens. The header comment
limits it to "Tie-breaker only" after the trigram prefilter. Queries
whose terms don't appear in any symbol's `signature + docs + attributes`
get filtered out by `bm25_rank.rs:92` and fall through to
`graph_zero_hit_fallback` (`graph_tools.rs:2392-2400`), which steers
the agent to grep.

**Call-graph confidence.** `ParsedCall` carries a `Confidence` field
(`lib.rs:148`). Direct calls get `Heuristic`, method calls
`CandidateSet`, macros `MacroOpaque` (see `languages/rust.rs:1770`,
`:1810`, `:1836`). The graph propagates this into `CallEdgeHit`, and
ambiguous method calls return a candidate fanout capped at
`CANDIDATE_FANOUT_LIMIT` (`graph_tools.rs:1394-1416`) so a query for
"callers of `Vec::push`" doesn't flood the agent with thousands of
hits.

## Cost intuition

For a single "find and explain function X" task on a 1500-line file
with a 50-line target function:

| Step | Graph-driven | Naïve |
|---|---|---|
| Locate | `definition_search` ≈ 400 tok | grep+read of N files ≈ 15000 tok |
| Shape | `read_slice {signature}` ≈ 50 tok | full file ≈ 7000 tok |
| Callers/refs | `symbol_context` ≈ 300 tok | grep+read of caller files ≈ 20000 tok |
| Implementation | `read_slice {body}` ≈ 400 tok | already paid above |
| **Total** | **~1150 tok** | **~42000 tok** |

The ratio compounds across the agent loop: a 10-turn task with 3–5
lookups per turn stays well under $0.05/turn at current Sonnet input
pricing on the graph path but easily exceeds $0.50/turn on the naïve
path, before counting waste from the model getting distracted by
unrelated code. And the signature/body split is itself the
optimisation: once a symbol is in the graph, fetching its signature is
a byte-range read, not a parse. The expensive operation is amortised
across every subsequent retrieval, and incremental parsing keeps that
amortisation cheap as the workspace evolves.
