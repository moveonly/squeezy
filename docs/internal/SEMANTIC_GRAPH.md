# Semantic Graph

Squeezy's semantic navigation layer is built from tree-sitter parsers, workspace
file records, and local resolution heuristics. It is designed to answer common
navigation questions before the model reads raw files.

## Design Policy: Graph-First Navigation

Squeezy deliberately keeps a graph-first navigation surface and will not retreat
to a bash/grep/`apply_patch` shell-loop navigation model. Tree-sitter grammars
for Rust, Python, Java, Kotlin, Scala, C#, Go, C/C++, JavaScript/TypeScript,
PHP, Ruby, Swift, and Dart drive a
persisted semantic graph with a typed-confidence call resolver and a documented
agent-facing tool surface (`decl_search`, `definition_search`,
`reference_search`, `hierarchy`, `upstream_flow`, `downstream_flow`,
`symbol_context`, `repo_map`). Lexical fallbacks (`grep`, `glob`, `read_slice`)
are intentionally framed as graph-anchored last resorts rather than the primary
navigation surface.

Squeezy does not adopt the alternative seen in adjacent shell-first agents
where tree-sitter is only used to parse `apply_patch` heredocs and the model is
expected to navigate code through `bash -lc` + grep + raw reads. That model
loses the multi-language symbol resolution, capped candidate sets, and typed
confidence per edge that this navigation layer exists to preserve.

Squeezy also declines to ship an LSP-backed navigation tool alongside the
tree-sitter graph. Adjacent agents expose a generic `lsp` tool that brokers
go-to-definition, hover, and completion requests to a per-language language
server (e.g. rust-analyzer, gopls, pyright). That shape conflicts with the
`AGENTS.md` rule "Do not use LSP or `rust-analyzer` for navigation" and would
fragment the evidence surface: tool callers would have to reconcile two
parallel confidence vocabularies (`ExactSyntax`/`ImportResolved`/... versus
LSP-reported precision), and `refresh_before_query` budgets would compete with
language-server cold-start and indexing time. Rust-analyzer and the JS/TS
language service remain benchmark oracles only — they expose Squeezy's losses
in `BENCHMARKS.md` rather than serving evidence on the production navigation
path. If a future request justifies LSP-shaped information (real type
inference, trait-dispatch resolution, generic substitution), the staged plan
is to model it as an explicit `compiler_facts`-style cache that records typed
facts into the existing graph confidence vocabulary, not as a passthrough tool
that forwards raw LSP responses to the model.

## What Is Indexed

- Gitignore-aware file records: path, relative path, size, mtime, stable content
  hash, language, and freshness.
- Policy-aware coverage for skipped generated, vendored, dependency cache,
  build output, lockfile, binary, large, VCS metadata, hidden, and
  user-excluded paths.
- Language-specific declaration coverage is tracked in
  [`../../crates/squeezy-skills/external-docs/LANGUAGES.md`](../../crates/squeezy-skills/external-docs/LANGUAGES.md).
  The supported `LanguageFamily` set is Rust, Python, Java, Kotlin, Scala, C#,
  Go, C/C++, JavaScript/TypeScript, PHP, Ruby, Swift, and Dart.
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
system roots such as `/`, `/System`, `/Library`, `/Users`, `/proc`, `/sys`,
`/run`, `/boot`, and `/snap` are blocking negative signals. Broad Linux package
and source roots such as `/opt`, `/usr`, and `/var` are not blocked solely by
name; they still need a strong project or source signal before indexing starts.
Personal folder names such as `Desktop`, `Documents`, and `Downloads` are weak
negative signals, but real project markers can override them. On case-sensitive
filesystems, near-miss project markers such as `cargo.toml` when `Cargo.toml`
was expected are reported as negative diagnostics instead of being silently
ignored. If there is no strong positive signal, or the root is a blocked
system/home root, Squeezy returns an empty graph with the indexing decision
instead of walking a likely non-code or dangerous directory.

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
- Language-specific heuristics, limitations, and follow-ups live in
  [`../../crates/squeezy-skills/external-docs/LANGUAGES.md`](../../crates/squeezy-skills/external-docs/LANGUAGES.md).
  This file only describes the shared graph policy and query surface.

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

Refresh is pending-event-first when callers provide authoritative changed
paths, with a bounded recrawl fallback otherwise. Callers that know paths
changed can call `record_changed_path`; the next graph query refreshes
immediately after debounce. Long-lived tool registries open the graph with
`GraphManager::open_watching`, which attaches the
`squeezy-graph::watcher::FileWatcher`. The watcher uses
`notify-debouncer-full` and OS-native backends (FSEvents, Linux inotify, or
ReadDirectoryChangesW) to queue debounced changed paths into the same pending
set. If native watcher registration fails, for example because Linux inotify
watch limits are exhausted or a mount cannot be watched recursively, Squeezy
falls back to a polling watcher and records the fallback reason. Graph tool
payloads expose the active `watcher_mode`, `watcher_backend`, and pending event
count so users and agents can distinguish native event refresh from fallback or
one-shot crawl-only graph managers. Refresh recrawls tracked files, compares
stable hashes, reparses changed files only, removes deleted files, and preserves
unchanged graph partitions.

The 250 ms per-tool refresh budget is a hard cap, not a soft hint. Reparse work
yields with `budget_exhausted=true` on the refresh report once the budget is
spent and leaves the remaining dirty files queued for the next tool call. This
keeps `decl_search`, `reference_search`, and other graph tools under a
predictable latency ceiling even when a branch switch or bulk edit dirties
hundreds of files, at the cost of letting a small tail of changes settle across
two or three tool calls instead of one. The alternative — lazy or streaming
refresh that serves partial results while reparse continues in the background —
was rejected because it would let stale symbols leak into evidence packets
with the same `freshness` label as fully-refreshed results, which the
typed-confidence contract does not allow. Benchmark harnesses or CI gates
that need a full settle should loop `refresh_before_query` until
`budget_exhausted` reports `false`, rather than treating a single budgeted
refresh as authoritative.

Body-only edits replace body-derived facts for that file.
Signature/module/import edits replace that file's stub and rebuild cross-file
indexes. JS/TS config edits such as `tsconfig.json` path changes or
`package.json` export changes rebuild the local resolver and dependent import
edges without reparsing unchanged source files.

Parsed graph partitions and resolver-cache snapshots are persisted in the
split graph `redb` database under the configured cache root
(`.squeezy/cache/graph.redb` by default). `state.redb` is reserved for
receipt metadata, read snapshots, observations, and small session-side cache
state. On a later session, unchanged files are hydrated from persisted
partitions and skip tree-sitter parsing; changed or missing partitions are
reparsed and written back. The graph store records a schema version plus
workspace, crawl-policy, language-registry, and graph-format metadata. A schema
mismatch backs up the old database and rebuilds fresh state instead of mutating
unknown data in place.

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

## Compiler Facts

Cargo is an explicit fact-refresh source, not a navigation dependency. The
`refresh_compiler_facts` tool requests compiler permission metadata (`cargo
facts:*`, or `cargo facts+check:*` when diagnostics are requested) and runs
`cargo metadata --format-version=1 --no-deps`; when requested, it also runs
`cargo check --message-format=json` to cache diagnostics. Navigation tools do
not invoke cargo. They only read the cached compiler facts already attached to
the in-memory graph.

The cargo fact cache records workspace, package, target, and feature nodes from
metadata, plus compiler diagnostics from JSON check output. Each batch stores
command provenance, cargo/rustc versions when available, capture time, and an
input fingerprint derived from Cargo manifests, lock/config files visible to the
graph, toolchain files such as `rust-toolchain.toml`, `build.rs` scripts, and
Rust source hashes. If those inputs change after refresh, the
`symbol_context.diagnostics` field still surfaces the cached diagnostics but
marks them stale via the per-hit freshness verdict.

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

Semantic graph benchmarks live under `benchmarks/`. The benchmark CLI supports
the same 13 `LanguageFamily` values as production indexing: Rust, Python, Java,
C#, Go, C/C++, JavaScript/TypeScript, PHP, Ruby, Kotlin, Swift, Scala, and
Dart. Smoke fixtures and query specs live under `benchmarks/fixtures/<family>/`
and `benchmarks/specs/`; `benchmarks/corpus.json` is the reproducible corpus
entry point for smoke and full-tier runs.

Current benchmark-only oracles are registered in
`benchmarks/squeezy-graph-bench/src/oracles.rs`: rust-analyzer, CPython AST,
javac, Kotlin compiler embeddable, Scala SemanticDB, Roslyn, Go parser/types
helpers, clang, TypeScript compiler API/language service, nikic/php-parser, Ruby
Prism, SourceKit-LSP, and the Dart analyzer. These oracles validate fixtures,
measure declaration accuracy, and expose navigation losses; production
navigation remains tree-sitter plus the local graph.

Mixed workloads generate deterministic scenarios from indexed symbols and
resolved call edges, then exercise hierarchy, symbol lookup, signature search,
body search, reference search, callers, callees, and call-chain queries.
Mixed-workload support is currently enabled for C, C++, C#, Go, JavaScript,
TypeScript, PHP, and Rust. Mixed timings are reported for trend analysis rather
than used as a hard gate.

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

Latest local benchmark snapshots and per-language result tables are documented
in [`BENCHMARKS.md`](BENCHMARKS.md). Keep time-sensitive result claims there,
not in this architecture note.
