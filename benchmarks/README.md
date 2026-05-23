# Semantic Graph Benchmarks

This directory contains end-to-end semantic graph benchmarks for `squeezy-cfa.4`.

The production graph runner uses Squeezy's Rust tree-sitter parser and in-memory
graph. The oracle tier validates each fixture with the Rust compiler and compares
Squeezy query output against checked query specifications. The oracle is a
benchmark/testing aid only; production navigation must not call `rustc`, LSP, or
`rust-analyzer`.

## Layout

```text
benchmarks/
  fixtures/rust/semantic-cases/     # small Rust crate used by smoke CI
  specs/smoke-queries.json          # expected query results and miss policy
  squeezy-graph-bench/              # benchmark CLI
```

## Local Run

```sh
cargo run --release --manifest-path benchmarks/squeezy-graph-bench/Cargo.toml -- \
  --fixture benchmarks/fixtures/rust/semantic-cases \
  --spec benchmarks/specs/smoke-queries.json \
  --report target/semantic-graph-benchmark/smoke.json \
  --mixed-repo .
```

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

## Accuracy Oracle

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

Reference and call-target TP/FP/FN are not yet externally validated. The local
rust-analyzer `search` command failed against the current toolchain, while SCIP
and rustc/HIR require a separate parser/oracle layer. Until that lands, call and
reference accuracy is covered by fixture specs plus documented limitations, not
by a full rust-analyzer result-set diff.

## Local Results

Latest local release run on May 23, 2026 with
`rust-analyzer 1.93.1 (01f6ddf7 2026-02-11)`, `--mixed-iterations 5000` for
external repos:

| Repo | Scenarios | Squeezy total | rust-analyzer total | Symbol TP | FP | FN | Refresh after 2 edits |
|---|---:|---:|---:|---:|---:|---:|---:|
| Squeezy | 4,696 | 116 ms | 3,814 ms | 399 | 0 | 0 | 33 ms |
| ripgrep | 5,000 | 646 ms | 5,317 ms | 3,837 | 0 | 0 | 227 ms |
| fd | 5,000 | 113 ms | 3,877 ms | 455 | 0 | 0 | 38 ms |
| bat | 5,000 | 380 ms | failed locally | 1,136 | 0 | 0 | 115 ms |
| tokio | 5,000 | 1,687 ms | 4,916 ms | 11,380 | 0 | 0 | 178 ms |
| serde | 5,000 | 635 ms | 7,697 ms | 3,512 | 0 | 0 | 197 ms |

Symbol accuracy scope for the same run:

| Repo | Comparable RA | Raw RA | Excluded RA | Comparable Squeezy | Raw Squeezy | Excluded Squeezy |
|---|---:|---:|---:|---:|---:|---:|
| Squeezy | 399 | 1,289 | 890 | 399 | 424 | 25 |
| ripgrep | 3,837 | 7,542 | 3,705 | 3,837 | 3,937 | 100 |
| fd | 455 | 1,045 | 590 | 455 | 478 | 23 |
| bat | 1,136 | 2,333 | 1,197 | 1,136 | 1,202 | 66 |
| tokio | 11,380 | 23,020 | 11,640 | 11,380 | 12,158 | 778 |
| serde | 3,512 | 5,985 | 2,473 | 3,512 | 3,720 | 208 |

Bat's `rust-analyzer analysis-stats` run failed locally because this
rust-analyzer/cargo combination passed `--lockfile-path` to cargo. The per-file
`rust-analyzer symbols` oracle still ran; one non-UTF-8 Rust snapshot was skipped
by both Squeezy and the oracle as unsupported input.

## CI

`.github/workflows/semantic-graph-benchmark.yml` runs the smoke benchmark on PRs
and pushes. `workflow_dispatch` with `tier=full` clones ripgrep, fd, bat, tokio,
and serde, runs 5,000 deterministic mixed-workload scenarios per repo, and
writes timing and symbol accuracy summaries to the GitHub Actions step summary.
