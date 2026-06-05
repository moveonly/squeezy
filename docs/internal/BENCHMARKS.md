# Semantic Graph Benchmarks

This directory contains end-to-end semantic graph benchmarks for Squeezy's
semantic graph.

The production graph runner uses Squeezy's tree-sitter parsers and in-memory
graph. The oracle tier validates each fixture with a slower language-specific
checker and compares Squeezy query output against checked query specifications.
The oracle is a benchmark/testing aid only; production navigation must not call
`rustc`, LSP, `rust-analyzer`, Python runtime analysis, Node, or TypeScript.

Language coverage, oracle ownership, mixed-workload support, limitations, and
language follow-ups are documented in
[`../../crates/squeezy-skills/external-docs/LANGUAGES.md`](../../crates/squeezy-skills/external-docs/LANGUAGES.md).

## Layout

```text
benchmarks/
  fixtures/rust/semantic-cases/     # small Rust crate used by smoke CI
  fixtures/python/semantic-cases/   # small Python package used by smoke CI
  fixtures/js-ts/semantic-cases/    # small JS/TS package used by smoke CI
  fixtures/java/semantic-cases/     # small Java package used by smoke CI
  fixtures/kotlin/semantic-cases/   # small Kotlin package used by smoke CI
  fixtures/scala/semantic-cases/    # small Scala package used by smoke CI
  fixtures/go/semantic-cases/       # small Go module used by smoke CI
  fixtures/c/semantic-cases/        # small C project used by smoke CI
  fixtures/cpp/semantic-cases/      # small C++ project used by smoke CI
  fixtures/php/semantic-cases/      # small PHP project used by smoke CI
  fixtures/ruby/semantic-cases/     # small Ruby project used by smoke CI
  fixtures/swift/semantic-cases/    # small Swift package used by smoke CI
  fixtures/dart/semantic-cases/     # small Dart package used by smoke CI
  corpus.json                       # pinned smoke/full corpus manifest
  specs/smoke-queries.json          # expected query results and miss policy
  specs/python-smoke-queries.json   # Python expected query results
  specs/js-ts-smoke-queries.json    # JS/TS expected query results
  specs/java-smoke-queries.json     # Java expected query results
  specs/go-smoke-queries.json       # Go expected query results
  specs/c-smoke-queries.json        # C expected query results
  specs/cpp-smoke-queries.json      # C++ expected query results
  specs/kotlin-smoke-queries.json
  specs/scala-smoke-queries.json
  specs/php-smoke-queries.json
  specs/ruby-smoke-queries.json
  specs/swift-smoke-queries.json
  specs/dart-smoke-queries.json
  squeezy-graph-bench/              # benchmark CLI
```

## Local Run

```sh
cargo run --release --manifest-path benchmarks/squeezy-graph-bench/Cargo.toml -- --list-languages
cargo run --release --manifest-path benchmarks/squeezy-graph-bench/Cargo.toml -- --list-oracles

cargo run --release --manifest-path benchmarks/squeezy-graph-bench/Cargo.toml -- \
  --corpus benchmarks/corpus.json \
  --family all \
  --tier smoke \
  --report-dir target/semantic-graph-benchmark

python3 benchmarks/scripts/summarize.py \
  --language all \
  --report-glob "target/semantic-graph-benchmark/**/*.json" \
  --output target/semantic-graph-benchmark/summary.md

cargo run --release --manifest-path benchmarks/squeezy-graph-bench/Cargo.toml -- \
  --language rust \
  --fixture benchmarks/fixtures/rust/semantic-cases \
  --spec benchmarks/specs/smoke-queries.json \
  --report target/semantic-graph-benchmark/rust-smoke.json \
  --ra-lsp-probes 25
```

Use `--language rust|python|java|kotlin|scala|go|c|cpp|csharp|javascript|typescript|js-ts|php|ruby|swift|dart`
with the matching fixture/spec from
`../../crates/squeezy-skills/external-docs/LANGUAGES.md`. Families with mixed
workload support also accept `--mixed-repo <path>` and `--mixed-iterations <n>`.

`benchmarks/corpus.json` is the reproducible v0 corpus entry point. It records
the smoke fixtures, full-tier external repos, pinned source commits, scenario
limits, oracle probe limits, and report paths. `--tier full` runs smoke plus
full cases for the selected family so CI and local release checks include the
small deterministic fixture before external corpus variance.

## CI Workflow

Benchmark CI is consolidated into:

- `.github/workflows/benchmark.yml`: orchestrator with one job per
  `LanguageFamily`.
- `.github/workflows/benchmark-lang.yml`: reusable `workflow_call` benchmark
  runner.
- `.github/actions/setup-bench/action.yml`: shared toolchain setup and Cargo
  cache.
- `benchmarks/scripts/summarize.py`: shared GitHub step summary writer.

The orchestrator preserves historical job display names for branch protection
compatibility and can be triggered with `workflow_dispatch` for `all` or a single
family.


The run fails if required expected results are missing, the fixture graph build
plus query time is not faster than compiler validation, or the incremental
refresh probe reparses more files than it edited.

Every smoke spec includes a `fallback_quality` query. The checked-in fixtures
carry generated and vendor-path source files, and the benchmark asserts those
paths are reported as fallback/exclusion evidence instead of being treated as
high-confidence graph answers. Reports also include deterministic tool/cost
metrics (`estimated_usd_micros = 0`), grep-baseline query counts, answer-quality
counts, and fallback/low-confidence rates.

The mixed workload is deterministic and exhaustive by default. It builds a
Squeezy graph for the supplied repo, generates scenarios from every indexed
symbol and resolved call edge, and runs hierarchy, symbol lookup, signature
search, body search, reference search, callers, callees, and call-chain queries.
Use `--mixed-iterations N` to cap the scenario count; `0` means run all
generated scenarios.

The benchmark also times `cargo check`, times
`rust-analyzer analysis-stats --run-all-ide-things` when available either on
`PATH` or through `rustup which rust-analyzer`, and then copies Rust files into a
temporary directory to measure refresh after editing two files. Mixed-workload
timing is reported but not used as a hard gate because CI machines vary and
rust-analyzer availability differs across toolchains.

## Accuracy Oracles

The benchmark runs `rust-analyzer symbols` for each Rust file and compares those
declarations with Squeezy's graph symbols. The report includes TP, FP, FN,
precision, recall, and examples for comparable declaration kinds:

- modules, structs, enums, unions, traits, impls
- functions, methods, consts, statics, type aliases, macros

Rust-analyzer locals, fields, and variants are excluded from TP/FP/FN because
Squeezy does not expose them as declaration symbols. The report still includes
raw and excluded counts so a perfect comparable-symbol score cannot hide the
filtered scope. Squeezy `#[test]` functions are normalized to functions,
unnamed `const _` items are not exposed as declarations, and rust-analyzer
`impl Foo` / `unsafe impl Foo` labels are normalized to `Foo` so the comparison
measures declaration discovery rather than display formatting.

The benchmark also starts `rust-analyzer` as a JSON-RPC LSP server and uses it as
a sampled navigation oracle:

- `textDocument/definition` probes sampled Squeezy call and macro edges and
  compares the resolved target with rust-analyzer's definition target.
- `textDocument/references` probes sampled declaration symbols and compares
  Squeezy `references_to_symbol` locations with rust-analyzer references.
- `--ra-lsp-probes N` caps the deterministic sample per repo; the default is 25
  and `0` disables the LSP oracle. Full CI runs 50 probes per external repo.

Navigation reports include available probe counts, sampled probe counts, TP, FP,
FN, precision, recall, and examples. Definition reports also include
`wrong_target` for real Squeezy target mismatches and `squeezy_only` for places
where Squeezy resolved a local-looking target but rust-analyzer returned no
definition. Reference probes exclude declarations because the selected symbol
already supplies the definition span. `references_to_symbol` is intentionally
more conservative than broad `reference_search`: it uses resolved call/reference
edges, type-context filters, package-local scoping, and declaration-name-span
checks to avoid same-name lexical false positives.

For Python, the benchmark runs a CPython `ast` oracle over the fixture and
compares class/function/method declarations against Squeezy's tree-sitter graph.
The oracle intentionally avoids executing user code; it is slower and more
accurate for declaration discovery than the production parser, but it does not
model dynamic attributes, metaclasses, import side effects, or runtime dispatch.
Files that CPython `ast` cannot parse are reported as `oracle_unparseable` and
excluded from Squeezy false-positive accounting so whole-repo fixture corpora do
not turn tree-sitter recovery from broken or future-syntax files into false
parser defects. The Python smoke spec also includes controlled navigation
queries for route attributes, property references, and constructor-alias method
calls; these are fixture oracles for syntax-only navigation behavior rather than
runtime framework checks.

For Java, the benchmark runs a JDK compiler tree API oracle when `java` is
available and compares class/interface/enum/record/method/constructor
declarations against Squeezy's tree-sitter graph. The oracle does not require
successful type attribution and is not a production dependency. The TP/FP/FN
numbers are declaration-only; they do not claim reference, call, dispatch,
overload, generated-code, annotation-processor, or classpath completeness. If no
JDK is available, the oracle is reported as skipped and the deterministic Java
query spec still gates parser/navigation behavior. Java reports also include a
fixture-query navigation oracle over `expected_contains` checks for references,
call chains, and Maven/Gradle project facts; this is a minimum expected set, so
per-query extras are reported but are not counted as false positives.

Latest local Java release run on May 23, 2026:

| Repo | Squeezy total | JDK oracle | TP | FP | FN | Precision | Recall |
|---|---:|---:|---:|---:|---:|---:|---:|
| junit5 | 2,719 ms | 1,159 ms | 18,890 | 4 | 8 | 0.9998 | 0.9996 |
| mockito | 805 ms | 843 ms | 8,928 | 0 | 0 | 1.0000 | 1.0000 |
| guava | 20,804 ms | 2,056 ms | 66,217 | 0 | 0 | 1.0000 | 1.0000 |
| retrofit | 502 ms | 643 ms | 3,505 | 0 | 0 | 1.0000 | 1.0000 |
| picocli | 7,269 ms | 880 ms | 9,523 | 0 | 0 | 1.0000 | 1.0000 |

For C#, the benchmark validates the smoke fixture with `dotnet build` and runs
the Roslyn syntax oracle in `benchmarks/oracle/csharp`. The oracle reports
declaration symbols plus syntactic extends/implements edges; it deliberately
does not claim overload resolution, dynamic dispatch, extension-method binding,
MSBuild-equivalent project evaluation, generated-code flow, or Razor embedded-C#
coverage. The C# smoke spec gates expected navigation and graph behavior for
declarations, attributes, body hits, references, partial-type calls,
inheritance/interface/partial edges, and .NET project facts. Razor, `.razor`,
and `.cshtml` files remain bounded fallback inputs for v0.

Latest local C# smoke run on May 24, 2026:

| Fixture | Squeezy total | dotnet build | Symbol TP | FP | FN | Edge TP | FP | FN |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| semantic-cases | 6 ms | 621 ms | 32 | 0 | 0 | 2 | 0 | 0 |

Latest local C# full-tier external run on May 24, 2026 used 1,500
deterministic mixed-workload scenarios per repo. The Roslyn oracle produced
symbol TP/FP/FN for all five repos; local `dotnet build` validation was
reporting-only and failed because this machine has .NET SDK 8.0.201 while some
repos pin .NET 10 SDKs or newer `.slnx` solution handling.

| Repo | Squeezy total | TP | FP | FN | Precision | Recall |
|---|---:|---:|---:|---:|---:|---:|
| automapper | 3,496 ms | 17,175 | 1,445 | 1,383 | 0.9224 | 0.9255 |
| dapper | 370 ms | 2,741 | 491 | 352 | 0.8481 | 0.8862 |
| newtonsoft_json | 3,601 ms | 9,357 | 4,829 | 3,652 | 0.6596 | 0.7193 |
| polly | 2,627 ms | 5,500 | 2,605 | 2,582 | 0.6786 | 0.6805 |
| serilog | 257 ms | 1,846 | 643 | 494 | 0.7417 | 0.7889 |

For C and C++, the benchmark validates source fixtures with `clang` or
`clang++ -fsyntax-only` and compares Squeezy declaration symbols with sampled
`clang -Xclang -ast-dump=json` output. This keeps compiler checking in the
benchmark tier while production navigation stays tree-sitter-only. Files that
need project-specific generated headers, compile flags, SDKs, or
`compile_commands.json` are reported as unparseable and excluded from Squeezy
false-positive accounting. The C/C++ query specs track high-coverage syntax
navigation for declarations, includes, references, calls, macro opacity,
templates, overload-prone calls, and header/source pairing. Known losses are
expected for preprocessor expansion, inactive conditional branches, template
instantiation, overload resolution, function pointers, virtual dispatch, ADL,
generated code, and external headers.

## Local Results

Earlier local release run on May 23, 2026 with
`rust-analyzer 1.93.1 (01f6ddf7 2026-02-11)`, `--mixed-iterations 5000` for
external repos:

| Repo | Scenarios | Squeezy total | rust-analyzer total | Symbol TP | FP | FN | Refresh after 2 edits |
|---|---:|---:|---:|---:|---:|---:|---:|
| ripgrep | 5,000 | 646 ms | 5,317 ms | 3,837 | 0 | 0 | 227 ms |
| fd | 5,000 | 113 ms | 3,877 ms | 455 | 0 | 0 | 38 ms |
| bat | 5,000 | 380 ms | failed locally | 1,136 | 0 | 0 | 115 ms |
| tokio | 5,000 | 1,687 ms | 4,916 ms | 11,380 | 0 | 0 | 178 ms |
| serde | 5,000 | 635 ms | 7,697 ms | 3,512 | 0 | 0 | 197 ms |

Symbol accuracy scope for the same run:

| Repo | Comparable RA | Raw RA | Excluded RA | Comparable Squeezy | Raw Squeezy | Excluded Squeezy |
|---|---:|---:|---:|---:|---:|---:|
| ripgrep | 3,837 | 7,542 | 3,705 | 3,837 | 3,937 | 100 |
| fd | 455 | 1,045 | 590 | 455 | 478 | 23 |
| bat | 1,136 | 2,333 | 1,197 | 1,136 | 1,202 | 66 |
| tokio | 11,380 | 23,020 | 11,640 | 11,380 | 12,158 | 778 |
| serde | 3,512 | 5,985 | 2,473 | 3,512 | 3,720 | 208 |

Bat's `rust-analyzer analysis-stats` run failed locally because this
rust-analyzer/cargo combination passed `--lockfile-path` to cargo. The per-file
`rust-analyzer symbols` oracle still ran; one non-UTF-8 Rust snapshot was skipped
by both Squeezy and the oracle as unsupported input.

Latest local LSP-oracle run on May 23, 2026 used
`--mixed-iterations 1000 --ra-lsp-probes 50`:

| Repo | Scenarios | Squeezy total | RA analysis total | Def probes | Def TP | FP | FN | Squeezy-only | Wrong target | Ref symbols | Ref TP | FP | FN |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep | 1,000 | 484 ms | 5,250 ms | 50 / 15,190 | 10 | 1 | 11 | 0 | 1 | 50 / 3,276 | 15 | 0 | 146 |
| fd | 1,000 | 74 ms | 3,745 ms | 50 / 2,122 | 5 | 1 | 19 | 1 | 0 | 50 / 389 | 24 | 1 | 119 |
| bat | 1,000 | 318 ms | failed locally | 50 / 7,082 | 0 | 2 | 0 | 2 | 0 | 50 / 972 | 0 | 36 | 0 |
| serde | 1,000 | 521 ms | 7,403 ms | 50 / 9,540 | 0 | 0 | 17 | 0 | 0 | 50 / 2,804 | 8 | 3 | 115 |
| tokio | 1,000 | 1,459 ms | 4,764 ms | 50 / 40,098 | 0 | 8 | 6 | 8 | 0 | 50 / 8,787 | 0 | 26 | 23 |

The latest run uses parallel tree-sitter parsing for batches of at least eight
files, with one parser per worker and deterministic serial graph merge/index
rebuild. Compared with the prior serial-parser run, cold graph build time moved
from 470 -> 334 ms on ripgrep, 69 -> 53 ms on fd, 318 -> 287 ms on bat,
371 -> 300 ms on serde, and 1,631 -> 1,186 ms on tokio. The two-file refresh
probe intentionally stays serial and remains flat.

The navigation FP/FN numbers are intentionally not zero. The latest
high-precision reference path removes the earlier same-name lexical explosion
but accepts lower recall until Cargo package resolution, cfg/feature evaluation,
trait dispatch, deref/autoref, and macro expansion are modeled. It now also
resolves strict `Self::name`, `Type::name`, and `module::name` direct calls when
there is one local syntactic target. That improves recall but can show up as
Squeezy-only against rust-analyzer's active cfg/target view; examples include
cfg-gated Tokio helpers and build-script/test utilities. Bat's rust-analyzer
project load is degraded on this local toolchain, so its LSP reference numbers
are treated as a noisy oracle result.

Follow-up reference-FN reduction on May 23, 2026 added grouped `use` tree
expansion, workspace package scoping for top-level crates, inline-module path
matching, guarded unit-struct constructor references, and cfg-gated trait impl
declaration filtering. On deterministic 5,000-scenario local runs with 50 LSP
reference probes, `ripgrep` moved from TP=15 FP=0 FN=146 to TP=31 FP=1 FN=130.
`serde` moved from TP=8 FP=3 FN=115 to TP=39 FP=6 FN=84. The remaining misses
are mostly associated type references, trait method calls through type
inference, active cfg/feature differences, proc-macro-generated references, and
cross-crate reexports that need Cargo metadata rather than syntax-only
heuristics.

A subsequent reference-FN pass on May 23, 2026 bound identifier references
inside import clauses back to their resolved import entries, and added
precision-scoped trait associated type projection matching. On deterministic
5,000-scenario local runs with 50 LSP reference probes, `ripgrep` moved from
TP=31 FP=1 FN=130 to TP=89 FP=1 FN=72. `serde` moved from TP=39 FP=6 FN=84 to
TP=49 FP=6 FN=74. A broader associated-type declaration-family heuristic was
tested but rejected because it increased serde reference FP from 6 to 158.

## CI

`.github/workflows/benchmark.yml` is the shared benchmark entry point for Rust,
Python, Java, Go, C/C++, C#, and JS/TS. PRs and pushes run language smoke jobs
for touched graph/parser/workspace/benchmark paths. Manual `workflow_dispatch`
adds a `language` selector (`rust`, `python`, `java`, `go`, `c-family`,
`csharp`, `js-ts`, or `all`) plus `tier=smoke|full`.

Full-tier external repos are sourced from `benchmarks/corpus.json`, not from
duplicated workflow shell lists. The manifest pins each repo to a specific
commit and the reusable workflow checks out those commits before running the
case. This keeps demo/website citations tied to auditable inputs instead of
whatever each upstream repository's default branch contains on a later day.

For Rust, the full tier clones ripgrep, fd, bat, tokio, and serde from the
manifest, runs 5,000 deterministic mixed-workload scenarios per repo, and writes
timing, symbol accuracy, and rust-analyzer LSP navigation accuracy summaries to
the GitHub Actions step summary.

For C/C++, the full tier clones redis, curl, sqlite, protobuf, and
nlohmann/json, then runs 1,000 deterministic mixed workload scenarios, refresh
probes, and a 10-file clang AST symbol sample against each repo. Pushing a
branch under `benchmark-full/**` runs the same full Rust and C-family tiers
after the workflow is available on the default branch. Clang/clang++ syntax
validation and sampled clang AST symbol TP/FP/FN are reported when they succeed,
but full-tier external repos are not blocked on compiler validation because real
C/C++ projects often need project-specific include paths, generated headers, or
compile command databases.

For C#, the full tier clones Newtonsoft.Json, Dapper, AutoMapper, Polly, and
Serilog, then runs 1,500 deterministic mixed-workload scenarios, refresh probes,
`dotnet build` validation when available, and Roslyn symbol/edge oracle
comparison in reporting-only mode with the fixture speed gate disabled. This is
the five-repo external corpus for C#; the PR/smoke tier remains the small
`benchmarks/fixtures/csharp/semantic-cases` fixture.

The workflow uploads the raw JSON reports and rendered summaries as
`semantic-graph-benchmark-*` artifacts so benchmark runs can be audited after
the job completes. `benchmarks/scripts/summarize.py --language all` renders the
cross-language matrix used for v0 demo and website claims: wall time, graph
queries, grep-baseline queries, mixed scenarios, missing checks, fallback rate,
low-confidence edges, and oracle precision/recall by case.

The old per-language workflow files were collapsed into the two-file benchmark
workflow listed above. `benchmark.yml` selects the family and tier, then calls
`benchmark-lang.yml` with the matching toolchain and corpus settings. Manual
`workflow_dispatch` accepts `family=all` or a single family plus `tier=smoke` or
`tier=full`; full-tier runs clone the pinned external corpus entries from
`benchmarks/corpus.json` and report external-corpus variance without turning
every compiler/oracle mismatch into a hard PR gate.

For Go, the benchmark runs a benchmark-only Go parser/AST oracle over `.go`
files and compares top-level declarations against Squeezy's tree-sitter graph.
The Go report includes symbol TP/FP/FN, precision, recall, unparseable files,
and heuristic-iteration notes. Production Go navigation does not call `gopls`,
`go list`, `go build`, or the Go oracle. The refresh probe is language-aware and
fails when a two-file Go edit causes more files to be reparsed than edited. The
Go FP/FN accuracy gate is enforced for the smoke fixture by default and is
relaxed by `--no-speed-gate` so external corpora can run in reporting-only mode
without blocking the workflow on upstream parser drift, generated files, or
build-tag differences. Go now runs through the shared benchmark workflow:
manual `workflow_dispatch` with `family=go` and `tier=full`, or a push to
`benchmark-full/**` after the workflow is present on the default branch, clones
gin, cobra, prometheus, etcd, and zap.

Initial local Go full run on May 23, 2026 used Go 1.26.3 and compared five
popular open-source Go repositories:

| Repo | Squeezy total | Go oracle | Symbol TP | FP | FN | Precision | Recall | Refresh after 2 edits |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| gin | 249 ms | 415 ms | 1,634 | 277 | 52 | 0.8550 | 0.9692 | 201 ms |
| cobra | 145 ms | 410 ms | 682 | 101 | 2 | 0.8710 | 0.9971 | 99 ms |
| prometheus | 5,147 ms | 622 ms | 12,561 | 2,794 | 320 | 0.8180 | 0.9752 | 747 ms |
| etcd | 3,157 ms | 759 ms | 11,233 | 1,222 | 1,133 | 0.9019 | 0.9084 | 361 ms |
| zap | 192 ms | 421 ms | 1,392 | 126 | 29 | 0.9170 | 0.9796 | 146 ms |

Every repo's refresh probe reparsed exactly the two edited files. Prometheus and
etcd are currently slower than the Go oracle on full-repo cold graph timing,
which makes them useful targets for the next Go heuristic/performance iteration.

The first Go heuristic iteration targeted the obvious FP/FN sources from that
run:

- local `var` / `const` declarations are no longer exposed as graph symbols
- `var (...)` and `const (...)` declaration lists are expanded instead of
  truncated
- blank identifier declarations (`_`) are ignored for symbol accuracy
- Go `type_alias` tree-sitter nodes are recorded as `TypeAlias`
- suite-style `_test.go` methods named `Test*`, `Benchmark*`, or `Fuzz*` are
  normalized to test functions for oracle comparison
- local declarations inside top-level function literals are not exposed as
  top-level graph symbols
- internal symlinked Go files are indexed so workspace behavior matches the Go
  AST oracle on repos such as etcd
- unresolved/candidate reference edges are not eagerly materialized, and large
  body-hit trigram indexes fall back to exact scanning at query time

Latest local Go full run on May 23, 2026 after that iteration:

| Repo | Squeezy total | Go oracle | Decl graph | Full graph | Symbol TP | FP | FN | Precision | Recall | Refresh after 2 edits |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| gin | 273 ms | 408 ms | 34 ms | 191 ms | 1,686 | 0 | 0 | 1.0000 | 1.0000 | 184 ms |
| cobra | 153 ms | 416 ms | 15 ms | 97 ms | 684 | 0 | 0 | 1.0000 | 1.0000 | 96 ms |
| prometheus | 4,708 ms | 961 ms | 462 ms | 3,649 ms | 12,881 | 0 | 0 | 1.0000 | 1.0000 | 520 ms |
| etcd | 3,089 ms | 572 ms | 336 ms | 2,440 ms | 12,366 | 0 | 0 | 1.0000 | 1.0000 | 461 ms |
| zap | 241 ms | 686 ms | 28 ms | 176 ms | 1,421 | 0 | 0 | 1.0000 | 1.0000 | 142 ms |

The Prometheus and etcd cold-build timings are slower than the Go AST oracle
because the two timings are not measuring equivalent work yet. The Go oracle is
declaration-only; Squeezy builds the full semantic graph and indexes references,
calls, body hits, and edges. After lazy reference/body-hit materialization,
Prometheus still produced 1,065,591 body hits, 799,504 references, 105,072
calls, and 267,663 graph edges. Etcd produced 655,099 body hits, 514,823
references, 68,540 calls, and 193,437 graph edges. The remaining performance
target is the full graph phase: declaration-only graph construction is already
below the Go oracle on those repos, while full graph construction remains above
it.
