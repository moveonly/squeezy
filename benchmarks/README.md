# Semantic Graph Benchmarks

This directory contains end-to-end semantic graph benchmarks for `squeezy-cfa.4`.

The production graph runner uses Squeezy's tree-sitter parsers and in-memory
graph. The oracle tier validates each fixture with a slower language-specific
checker and compares Squeezy query output against checked query specifications.
The oracle is a benchmark/testing aid only; production navigation must not call
`rustc`, LSP, `rust-analyzer`, Python runtime analysis, Node, or TypeScript.

## Layout

```text
benchmarks/
  fixtures/rust/semantic-cases/     # small Rust crate used by smoke CI
  fixtures/python/semantic-cases/   # small Python package used by smoke CI
  fixtures/js-ts/semantic-cases/    # small JS/TS package used by smoke CI
  specs/smoke-queries.json          # expected query results and miss policy
  specs/python-smoke-queries.json   # Python expected query results
  specs/js-ts-smoke-queries.json    # JS/TS expected query results
  squeezy-graph-bench/              # benchmark CLI
```

## Local Run

```sh
mkdir -p target/benchmark-repos
git clone --depth 1 https://github.com/BurntSushi/ripgrep target/benchmark-repos/ripgrep

cargo run --release --manifest-path benchmarks/squeezy-graph-bench/Cargo.toml -- \
  --fixture benchmarks/fixtures/rust/semantic-cases \
  --spec benchmarks/specs/smoke-queries.json \
  --report target/semantic-graph-benchmark/smoke.json \
  --mixed-repo target/benchmark-repos/ripgrep \
  --mixed-iterations 1000 \
  --ra-lsp-probes 25
```

Python smoke:

```sh
cargo run --release --manifest-path benchmarks/squeezy-graph-bench/Cargo.toml -- \
  --language python \
  --fixture benchmarks/fixtures/python/semantic-cases \
  --spec benchmarks/specs/python-smoke-queries.json \
  --report target/semantic-graph-benchmark/python-smoke.json \
  --ra-lsp-probes 0
```

Python external oracle comparison:

```sh
cargo run --release --manifest-path benchmarks/squeezy-graph-bench/Cargo.toml -- \
  --language python \
  --fixture target/benchmark-repos/requests/src/requests \
  --spec benchmarks/specs/empty-queries.json \
  --report target/semantic-graph-benchmark/python-real/requests.json \
  --ra-lsp-probes 0 \
  --no-speed-gate
```

JS/TS smoke:

```sh
cargo run --release --manifest-path benchmarks/squeezy-graph-bench/Cargo.toml -- \
  --language typescript \
  --fixture benchmarks/fixtures/js-ts/semantic-cases \
  --spec benchmarks/specs/js-ts-smoke-queries.json \
  --report target/semantic-graph-benchmark/js-ts-smoke.json \
  --ra-lsp-probes 0 \
  --no-speed-gate
```

When the Node `typescript` package is installed, the JS/TS benchmark also runs a
benchmark-only TypeScript compiler API declaration oracle and reports symbol
TP/FP/FN. If TypeScript is unavailable, the report records that status
explicitly and still validates the tree-sitter query spec.

JS/TS full-tier comparison uses five representative open-source repositories:
Vite, Redux, Axios, Express, and Prettier. A local May 23, 2026 run with the
TypeScript compiler API oracle produced:

| Repo | Squeezy total | TS oracle | Symbol TP | FP | FN | Precision | Recall |
|---|---:|---:|---:|---:|---:|---:|---:|
| axios | 61 ms | 190 ms | 865 | 3 | 30 | 0.9965 | 0.9665 |
| express | 23 ms | 150 ms | 297 | 0 | 3 | 1.0000 | 0.9900 |
| prettier | 385 ms | 327 ms | 4,506 | 0 | 127 | 1.0000 | 0.9726 |
| redux | 14 ms | 159 ms | 130 | 0 | 3 | 1.0000 | 0.9774 |
| vite | 672 ms | 387 ms | 6,807 | 3 | 347 | 0.9996 | 0.9515 |

The run fails if required expected results are missing, the fixture graph build
plus query time is not faster than compiler validation, or the incremental
refresh probe reparses more files than it edited.

The mixed workload is deterministic and exhaustive by default. It builds a
Squeezy graph for the supplied Rust repo, generates scenarios from every indexed
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

`.github/workflows/semantic-graph-benchmark.yml` runs the smoke benchmark on PRs
and pushes. `workflow_dispatch` with `tier=full` clones ripgrep, fd, bat, tokio,
and serde, runs 5,000 deterministic mixed-workload scenarios per repo, and
writes timing, symbol accuracy, and rust-analyzer LSP navigation accuracy
summaries to the GitHub Actions step summary. The workflow also uploads the raw
JSON reports and rendered summary as the `semantic-graph-benchmark-<tier>`
artifact so benchmark runs can be audited after the job completes.

`.github/workflows/python-semantic-graph-benchmark.yml` runs the Python smoke
benchmark on PRs and pushes that touch graph, parser, workspace, benchmark, or
workflow files. It writes the Squeezy timing, CPython AST oracle accuracy, query
diffs, and raw JSON report as a workflow artifact. Manual `workflow_dispatch`
with `tier=full` additionally clones requests, flask, click, black, and fastapi,
then runs oracle-only FP/FN comparison against their importable package source
roots with the fixture speed gate disabled so external-corpus variance is
reported rather than blocking the run. The full tier intentionally avoids whole
repository roots because formatter snapshots, parser fixtures, and future-syntax
test corpora can contain Python files that tree-sitter can recover from but
CPython `ast` rejects as non-modules. If such files appear anyway, the report
counts them as `oracle_unparseable` instead of Squeezy false positives.
