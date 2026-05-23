# Semantic Graph

Squeezy's semantic navigation layer is built from tree-sitter parsers, workspace
file records, and local resolution heuristics. It is designed to answer common
navigation questions before the model reads raw files.

## What Is Indexed

- Gitignore-aware file records: path, relative path, size, mtime, stable content
  hash, language, and freshness.
- Policy-aware coverage for skipped generated, vendored, dependency cache,
  build output, lockfile, binary, large, VCS metadata, hidden, and
  user-excluded paths.
- Rust declarations: modules, structs, enums, unions, traits, impls, functions,
  methods, consts, statics, type aliases, macros, and tests.
- Python declarations: classes, functions, methods, imports, calls, decorators,
  docstrings, class bases, type annotations, class fields, exports, aliases, and
  references from `.py` files.
- C/C++ declarations: includes, namespaces, classes, structs, unions, enums,
  typedefs/type aliases, fields, functions, methods, constructors/destructors,
  operators, templates, macro definitions/usages, and declaration/definition
  spans.
- Go declarations: packages, imports, structs, interfaces, type aliases,
  functions, methods, receiver relationships, fields, constants, variables,
  tests, calls, and references from `.go` files.
- Signatures: raw item header, visibility, attributes, docs, spans, and body
  spans where present.
- Edges: containment, imports/reexports, references, calls, and macro
  invocations.
- Body hits: identifiers, type names, paths, calls, macro invocations, literals,
  and attributes scoped to the nearest owning symbol.

Unsupported files are retained as structured unsupported results so callers can
fall back to bounded read/grep/list navigation without pretending the graph knows
more than it does.

Generated, vendored, dependency cache, build output, binary, lockfile, and large
files are excluded from graph indexing by default with compact reason-tagged
coverage. Directory-level exclusions such as `vendor/`, `node_modules/`, and
`target/` are pruned at the walker so individual files inside them are never
visited; the pruned directory shows up once in the coverage report rather than
once per file. Unrecognized hidden paths are skipped when `include_hidden=false`
and counted under the `hidden` reason. Project config can re-include paths via
`include = [...]` or whole classes via `include_classes = ["lockfile"]`; when an
include glob points below a default-excluded directory the crawler walks into
that directory so the glob can match. `exclude_classes = [...]` is the
counterpart that keeps a class pruned even when an include glob would otherwise
re-enable it. Explicit fallback tool reads can still inspect excluded files
through the ignored-search permission and normal byte budgets; `read_file`
returns `ignored=true` plus an `ignored_reason` for those reads.

## Heuristics

Workspace indexing starts with a defensive root decision before any recursive
walk. VCS markers such as `.git`, `.jj`, `.hg`, and `.svn`, common project
config, and shallow source files are strong positive signals. A root `README` is
recorded as a weak positive signal, but is not enough on its own. Source signals
scan to depth two with a small entry cap and include Rust, Python, Java, C#,
C/C++, JavaScript, TypeScript, Go, Ruby, PHP, Swift, Kotlin, Scala, shell, and
common web files. Common code directories such as `src`, `lib`, `app`,
`packages`, `crates`, `cmd`, `pkg`, `internal`, and `include` are positive only
when they contain shallow source files. The user's home directory and protected
system roots such as `/`, `/System`, `/Library`, `/Users`, `/usr`, and `/var`
are blocking negative signals. Personal folder names such as `Desktop`,
`Documents`, and `Downloads` are weak negative signals, but real project markers
can override them. If there is no strong positive signal, or the root is a
blocked system/home root, Squeezy returns an empty graph with the indexing
decision instead of walking a likely non-code or dangerous directory.

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
- Python constructor calls resolve to local class declarations when the class is
  same-file or imported unambiguously. `import x as y`, `from x import y as z`,
  leading-dot relative imports, package `__init__.py` reexports, `__all__`, and
  simple assignments such as `Alias = Real` or `obj = ClassName()` feed the same
  import/alias resolver. Alias facts are scoped to their owning symbol and
  receiver aliases use the latest assignment before the call site.
- Python method calls use confidence-ranked heuristics: same class first,
  inherited local classes next, constructor-derived receiver aliases after that,
  and imported module-qualified functions such as `helpers.build()` when the
  target file path matches the imported module. Call edges include a `rank=...`
  provenance reason such as `same file`, `explicit import`, `inherited class`,
  `constructor alias`, `imported module`, or `package local`.
- Python class bases and function annotations are indexed as type references.
  Annotation facts are soft evidence only; no mypy-style type solving, generic
  constraint solving, or runtime import execution is attempted.
- Python decorators are kept as raw attributes and tagged for common semantics:
  `@property`, `@staticmethod`, `@classmethod`, dataclasses, pytest fixtures,
  FastAPI/Flask-style route decorators with method/path metadata, and common
  Pydantic validators. Property attribute reads such as `self.name` can bind to
  local `@property def name(...)` methods.
- Python class-level annotated assignments and field factories are indexed as
  fields. This covers dataclass/Pydantic-style fields, SQLAlchemy
  `Column(...)`/`mapped_column(...)`, and Django `models.*Field(...)` syntax
  without importing those frameworks.
- Python test functions/classes are tagged from `test_*`, `*_test.py`, pytest
  fixtures, and `Test*` class naming. Docstring text is stored on the owning
  symbol so behavior-word searches do not have to read raw files first.
- Dynamic attributes, metaclasses, runtime import side effects, monkey-patching,
  and type-inferred receiver dispatch remain heuristic or external.
<<<<<<< HEAD
- C/C++ header classification prefers same-stem source files and then project
  majority when a plain `.h` file has no unambiguous pair. `.hpp`, `.hh`, and
  `.hxx` are treated as C++.
- C/C++ include directives are indexed as glob import facts. Cross-translation-unit
  direct calls are resolved through includes: when `#include "header.h"` is
  visible and the called name resolves to a single Function/Method declared in
  the included header (or a sibling translation unit sharing the header's
  stem/directory), the call binds to the definition with `ImportResolved`
  confidence; ambiguous matches stay candidate-set. Declaration/definition
  pairing and calls also use structural heuristics: namespace/class scope, name,
  arity, receiver text, and normalized signature tokens. Sibling method calls
  inside a class without `this->` (parsed as Direct in tree-sitter-cpp) resolve
  to the same-class peer method when unambiguous. Overloads, templates,
  function pointers, virtual dispatch, ADL, and macro-dependent calls produce
  candidate-set, partial, macro-opaque, or conditional confidence instead of
  exact claims.
- C/C++ `using foo::Name;` declarations and `using namespace foo;` directives
  are indexed as import facts so cross-namespace references and calls in real
  C++ code can resolve via the same import machinery.
- C/C++ function-pointer struct fields (`int (*cb)(int)`) are kept as `Field`
  symbols rather than promoted to `Function`/`Method`, matching how clang's
  AST reports them.
- C/C++ namespace-qualified free function definitions (`void ns::func() {}`)
  remain `Function` symbols; only class-qualified definitions
  (`void Foo::bar() {}`) are promoted to `Method`. The qualifier-leaf
  type-name heuristic (uppercase-leading or `_t`-suffix) is the cheap
  syntactic distinguisher.
- C++ template specializations (`template<> class Foo<int> {}`) are tagged
  with `c++:template-specialization` and excluded from the comparable-symbol
  count so they do not appear as false positives against the clang AST
  oracle (which reports them as `ClassTemplateSpecializationDecl`).
- C/C++ forward declarations and matching definitions in the same file
  collapse into a single canonical Function/Method symbol so the
  declaration-symbol count stays aligned with the clang AST oracle.
- C++ access modifiers resolve through `public:` / `private:` / `protected:`
  blocks. Aggregate defaults apply for members declared before the first
  access specifier (`struct`/`union` default to public, `class` defaults to
  private).
- C/C++ preprocessor directives are indexed but not expanded. Macro definitions,
  macro invocations, and conditional spans are provenance-bearing evidence for
  fallback, not compiler-equivalent semantics. All-caps call targets at least
  two characters long are flagged as macro-opaque so common macro-like APIs
  (`ASSERT`, `LOG`, `EXPECT_EQ`, `CHECK`) widen the macro-opaque cone instead
  of pretending to be direct function calls.
- Go package-qualified calls resolve through explicit imports when the imported
  package maps to one indexed package and one function target. Same-package
  direct calls and same-receiver method calls resolve when there is a single
  syntactic target. Interface satisfaction, full receiver type inference,
  embedded field promotion, build tags, generated code, and external modules are
  reported as candidate, heuristic, or external facts rather than guessed.

Every result carries a confidence label such as `ExactSyntax`, `ImportResolved`,
`Heuristic`, `CandidateSet`, `External`, `MacroOpaque`, `ConditionalUnknown`,
`Unsupported`, `Stale`, or `Partial`.

## Refresh Policy

`GraphManager` owns a workspace crawler, parser cache, and immutable graph
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

Semantic graph benchmarks live under `benchmarks/`. The Rust smoke benchmark
validates the fixture crate with the Rust compiler, builds the Squeezy graph,
runs query specs, and writes a JSON report. The Python smoke benchmark validates
the fixture with a slower CPython `ast` oracle and compares declaration symbols
against Squeezy's graph. Both fail if required expected results are missing or if
Squeezy graph build plus query time is not faster than the validation pass for
the same fixture.

The C and C++ smoke benchmarks validate fixtures with `clang -fsyntax-only` and
`clang++ -fsyntax-only`, then compare declaration symbols against
`clang -Xclang -ast-dump=json` output before running the same graph/query/spec
harness. Clang is a benchmark oracle only; production C/C++ navigation remains
tree-sitter and local graph analysis. External mixed benchmarks cap sampled
oracle files by default and exclude unparseable files from Squeezy
false-positive accounting because real projects often require generated
headers, compile flags, SDKs, or compile command databases. Known misses must be
documented for macros, inactive preprocessor branches, templates, overloads,
generated code, external headers, function pointers, and virtual dispatch.

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

Python accuracy reporting uses the CPython `ast` oracle as the slower reference
for class/function/method declaration discovery. It is benchmark-only and does
not become a production dependency. Python files that CPython `ast` cannot parse
are reported as `oracle_unparseable` and excluded from Squeezy false-positive
accounting, because tree-sitter recovery is useful while editing broken or
future-syntax code even when the oracle cannot treat that file as a module. The
Python smoke benchmark also carries controlled navigation checks for route
metadata, constructor-alias method calls, and property references so navigation
heuristics are regression-tested separately from declaration accuracy.

Go accuracy reporting uses a benchmark-only Go parser/AST oracle for
declaration discovery. It reports symbol TP/FP/FN, precision, recall, examples,
and heuristic-iteration notes so receiver/import/interface heuristics can be
accepted or rejected by measured FP/FN movement. The Go oracle is not a
production dependency and `gopls` is not used for production navigation.
The Go oracle is declaration-only; Squeezy cold-build timing currently includes
full graph work such as body-hit, reference, call, and edge materialization. Any
repo where Squeezy is slower than the Go oracle should be treated as a graph
build performance target, not as proof that the parser path is heavier than
Go's AST parser.

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
- Internal symlinked Go files are indexed when their resolved target stays
  inside the workspace root, matching Go parser behavior on repos such as etcd
  without indexing arbitrary external paths.
- Body-hit trigram indexing is disabled and `body_search` falls back to an
  exact lower-case scan whenever the workspace contains more than 100,000 body
  hits. The threshold is language-agnostic and applies to Rust, Python, and Go
  alike; large repos in any supported language will report
  `body_hit_trigram_indexed=false` in `GraphStats`/benchmark output so callers
  can correlate slower body searches with the fallback.
- Go generated files and remaining full-graph reference/call resolution work
  still need reduced or lazy indexing before full graph cold builds can be
  compared fairly with declaration-only oracles.
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
