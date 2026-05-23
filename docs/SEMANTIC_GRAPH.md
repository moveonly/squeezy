# Semantic Graph For Rust

Squeezy's first semantic navigation layer is a Rust graph built from
`tree-sitter-rust`, workspace file records, and local resolution heuristics. It is
designed to answer common navigation questions before the model reads raw files.

## What Is Indexed

- Gitignore-aware file records: path, relative path, size, mtime, stable content
  hash, language, and freshness.
- Rust declarations: modules, structs, enums, unions, traits, impls, functions,
  methods, consts, statics, type aliases, macros, and tests.
- Signatures: raw item header, visibility, attributes, docs, spans, and body
  spans where present.
- Edges: containment, imports/reexports, references, calls, and macro
  invocations.
- Body hits: identifiers, type names, paths, calls, macro invocations, literals,
  and attributes scoped to the nearest owning symbol.

Unsupported files are retained as structured unsupported results so callers can
fall back to bounded read/grep/list navigation without pretending the graph knows
more than it does.

## Heuristics

- Direct calls resolve when the target is same-file, explicitly imported, or
  syntactically qualified as `Self::name`, `Type::name`, or `module::name` with
  one matching local candidate.
- `self.method()` and sibling method calls inside an impl resolve to the same impl
  first.
- Other method calls return candidate sets when multiple methods share the name.
- Imports and reexports resolve aliases and simple paths when unambiguous; glob
  imports remain candidate sets.
- References use a funnel: lexical/body-index prefilter, AST context, then local
  symbol-name resolution.
- External Rust roots such as `std::`, `core::`, `alloc::`, and `proc_macro::`
  are not collapsed to same-name local symbols, including leaf identifiers inside
  those scoped paths.
- Macro calls are recorded but not expanded. Item-position macros, derive macros,
  attribute macros, and proc macros are treated as opaque or partial.
- Unknown cfg and feature combinations are not silently dropped; callers should
  treat affected results as lower-confidence until compiler facts land in the
  later compiler-as-fact epic.

Every result carries a confidence label such as `ExactSyntax`, `ImportResolved`,
`Heuristic`, `CandidateSet`, `External`, `MacroOpaque`, `ConditionalUnknown`,
`Unsupported`, `Stale`, or `Partial`.

## Refresh Policy

`GraphManager` owns a workspace crawler, Rust parser cache, and immutable graph
query surface. Before graph tool calls, callers should invoke
`refresh_before_query()`.

Defaults:

- debounce: 500 ms
- idle refresh interval: 15 seconds
- per-tool refresh budget: 250 ms

Refresh is event-first with a polling fallback. A file watcher or tool layer
should call `record_changed_path` when it sees an edit; the next graph query
refreshes immediately after debounce. If no events arrive, Squeezy polls every
15 seconds as a safety net. Refresh recrawls tracked files, compares stable
hashes, reparses changed files only, removes deleted files, and preserves
unchanged graph partitions. Body-only edits replace body-derived facts for that
file. Signature/module/import edits replace that file's stub and rebuild
cross-file indexes.

Tree-sitter parse work is parallelized for batches of at least eight files. Each
worker owns its own parser instance, and the final graph merge plus index rebuild
remain serial so output ordering and graph IDs stay deterministic. Small
refreshes, including the common one- or two-file edit case, stay serial to avoid
thread setup overhead.

Graph build and refresh reports include duration, file counts, reparsed byte
counts, symbol/edge counts, and Rust/supported/unsupported/unknown language
distribution. Telemetry callers use these reports for one-shot graph build
events and repeated graph refresh events without sending paths or source text.

## Traversal Surface

The in-memory graph supports:

- hierarchy traversal with bounded depth
- signature search by text, kind, visibility, and attribute
- body search by text, owner kind, and hit kind
- reference search by text/path segment
- callers, callees, and bounded call chains

The graph returns compact symbol and edge records with spans, freshness,
confidence, and provenance. Raw file reads should be targeted by these spans.

## Benchmarks

Semantic graph benchmarks live under `benchmarks/`. The smoke benchmark validates
the fixture crate with the Rust compiler, builds the Squeezy graph, runs query
specs, and writes a JSON report. It fails if required expected results are
missing or if Squeezy graph build plus query time is not faster than compiler
validation for the same fixture.

The mixed benchmark runs deterministic exhaustive scenarios against a real Rust
repo by default. It generates scenarios from every indexed symbol and resolved
call edge, then exercises hierarchy, symbol lookup, signature search, body
search, reference search, callers, callees, and call-chain queries. It also times
`cargo check`, optionally times
`rust-analyzer analysis-stats --run-all-ide-things`, and copies Rust files into a
temporary directory to measure refresh after editing two files.
Mixed-workload timings are reported for trend analysis rather than used as a
hard gate.

Accuracy reporting has two external rust-analyzer oracles. `rust-analyzer
symbols` compares comparable declaration families and reports symbol TP/FP/FN,
precision, recall, examples, raw counts, and excluded counts. Rust-analyzer
locals, fields, and variants are excluded from symbol TP/FP/FN because the
current Squeezy graph does not expose them as declaration symbols.

The benchmark also starts rust-analyzer as an LSP server for sampled navigation
diffs. `textDocument/definition` validates sampled Squeezy call and macro edge
targets, while `textDocument/references` compares sampled declaration references
against Squeezy `references_to_symbol`. This is intentionally a loss tracker
rather than a hard product dependency: it exposes wrong targets,
rust-analyzer-only definitions, and Squeezy-only extras while keeping production
navigation tree-sitter-only.

Known misses must be documented in the query spec with a reason, for example
macro expansion, trait dispatch, type inference, cfg, glob ambiguity, generated
code, or unresolved external code.

Current external-oracle gaps and known losses:

- the LSP oracle is sampled by `--ra-lsp-probes`, not exhaustive by default
- broad lexical `reference_search` remains high-recall and noisy; the
  symbol-aware `references_to_symbol` path is package-local, excludes
  declarations, and favors precision over recall
- receiver method calls do not bind to unique same-name local methods unless
  they are in the same impl, avoiding common wrong targets such as `get`,
  `push`, or `clear`
- strictly qualified direct calls such as `Self::from_arg_matches`,
  `Sender::from_mio`, and `module::render_template` are resolved when a single
  local syntactic target exists; these can appear as Squeezy-only LSP results
  when rust-analyzer's active cfg/target view does not include the site
- cross-package references are conservative until Cargo package/dependency facts
  are indexed
- item-generating macros and proc macros are recorded as opaque unless the
  generated item appears in source
- cfg/feature matrices are not enumerated
- trait dispatch, generic bounds, type inference, deref/autoref method
  resolution, and external crate/stdlib references remain heuristic or
  lower-confidence

Latest local benchmark snapshot is documented in `benchmarks/README.md`. On the
May 23, 2026 release run, comparable declaration symbols were 100% TP with 0 FP
and 0 FN against `rust-analyzer symbols` on five external popular Rust repos:
ripgrep, fd, bat, tokio, and serde. The LSP navigation oracle does show losses:
sampled references now have much lower FP counts in the symbol-aware path, but
FN counts remain high because unresolved cross-package, cfg/feature,
trait/deref/autoref, macro, and external references are not guessed.
