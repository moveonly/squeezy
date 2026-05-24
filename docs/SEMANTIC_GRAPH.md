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
- Language-specific declaration coverage is tracked in
  [`docs/LANGUAGES.md`](LANGUAGES.md). The supported `LanguageFamily` set is
  Rust, Python, Java, C#, Go, C/C++, and JavaScript/TypeScript.
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
scan to depth two with a small entry cap and include every `LanguageFamily` in
`squeezy-core`, plus Ruby, PHP, Swift, Kotlin, Scala, shell, and common web
files. Common code directories such as `src`, `lib`, `app`,
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
- Language-specific heuristics, limitations, and TODOs live in
  [`docs/LANGUAGES.md`](LANGUAGES.md). This file only describes the shared graph
  policy and query surface.

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
cross-file indexes. JS/TS config edits such as `tsconfig.json` path changes or
`package.json` export changes rebuild the local resolver and dependent import
edges without reparsing unchanged source files.

Parsed graph partitions are persisted in a `redb` database under the configured
cache root (`.squeezy/cache/state.redb` by default). On a later session,
unchanged files are hydrated from persisted partitions and skip tree-sitter
parsing; changed or missing partitions are reparsed and written back. The store
records a schema version plus workspace, crawl-policy, language-registry, and
graph-format metadata. A schema mismatch backs up the old database and rebuilds
fresh state instead of mutating unknown data in place.

Tree-sitter parse work is parallelized for batches of at least eight files. Each
worker owns its own parser instance, and the final graph merge plus index rebuild
remain serial so output ordering and graph IDs stay deterministic. Small
refreshes, including the common one- or two-file edit case, stay serial to avoid
thread setup overhead.

Graph build and refresh reports include duration, file counts, persisted
partition hit/miss counts, reparsed byte counts, symbol/edge counts, and
Rust/Python/JS/TS/supported/unsupported/unknown language distribution. Refresh
reports also separate changed paths observed from events, changed paths
discovered by polling, and unchanged event paths so file-watcher FP/FN behavior
can be benchmarked without sending paths or source text. Telemetry callers use
these reports for one-shot graph build events and repeated graph refresh events
without sending paths or source text.

## Traversal Surface

The in-memory graph supports:

- hierarchy traversal with bounded depth
- signature search by text, kind, visibility, and attribute
- body search by text, owner kind, and hit kind
- reference search by text/path segment
- callers, callees, and bounded call chains

The agent-facing graph tool surface is:

- `repo_map` for compact architecture maps, language counts, coverage, and
  unsupported-file samples
- `decl_search` and `definition_search` for declaration lookup and
  disambiguation
- `reference_search` for symbol-bound or broad heuristic references
- `upstream_flow` and `downstream_flow` for callers, callees, references, and
  bounded call-chain context
- `symbol_context` and `hierarchy` for focused symbol/module exploration
- `read_slice` for exact bounded source slices from graph spans or explicit
  byte/line ranges, plus `read_mode="diff"` for changed ranges against
  `worktree`, `branch_base`, `index`, or `last_receipt` baselines. The
  changed-range schema is line/byte hunks derived from git today; a
  graph-driven variant that expands each changed range to the smallest
  enclosing symbol span (function, method, impl, test) is a follow-up rather
  than something this surface already implements.

Graph navigation tools return uniform evidence packets with `claim`, `spans`,
`confidence`, `freshness`, `provenance`, `cost_hint`, and `next_action`.
Unsupported or unknown-language paths return structured fallback suggestions for
bounded grep/read navigation rather than graph confidence. Raw file reads should
be targeted by graph spans or `read_slice` ranges.

## Benchmarks

Semantic graph benchmarks live under `benchmarks/`. The Rust smoke benchmark
validates the fixture crate with the Rust compiler, builds the Squeezy graph,
runs query specs, and writes a JSON report. The Python smoke benchmark validates
the fixture with a slower CPython `ast` oracle and compares declaration symbols
against Squeezy's graph. The Java smoke benchmark uses the JDK compiler tree API
as a benchmark-only declaration oracle when `java` is available, and still runs
deterministic query gates when it is not. The language smoke benchmarks fail if
required expected results are missing or, when a validation oracle is available,
Squeezy graph build plus query time is not faster than the validation pass for
the same fixture.

The JS/TS smoke benchmark validates a controlled TSX fixture with query specs.
When the `typescript` npm package is available, the benchmark also runs three
oracle tiers: a declaration oracle (TypeScript compiler API, reports symbol
TP/FP/FN for file/name/kind declarations), a mixed workload (all nine query
types against real repos with a refresh probe), and a navigation oracle
(TypeScript Language Service `getDefinitionAtPosition` / `findReferences` probes
on sampled call-edge sites and declaration symbols — the JS/TS equivalent of the
rust-analyzer LSP probes on the Rust benchmark). If Node or TypeScript is
unavailable the report records that status explicitly and still runs the
tree-sitter query spec.

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

Java accuracy reporting uses the JDK compiler tree API as the reference for
class/interface/enum/record/method/constructor declaration discovery. It is
benchmark-only and does not become a production dependency. Java FP/FN counts
are declaration-only; they do not prove reference, call, dispatch, overload, or
classpath completeness. The Java smoke spec carries controlled fixture truth
for imports, constructor calls, field-receiver method calls,
inheritance/interface references, package-local symbols, and Maven/Gradle
project facts. The query oracle is an `expected_contains` minimum oracle; extra
results stay visible per query but are not counted as false positives.

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
