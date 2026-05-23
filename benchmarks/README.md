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
  Squeezy `reference_search` locations with rust-analyzer references.
- `--ra-lsp-probes N` caps the deterministic sample per repo; the default is 25
  and `0` disables the LSP oracle. Full CI runs 50 probes per external repo.

Navigation reports include available probe counts, sampled probe counts, TP, FP,
FN, precision, recall, and examples. Definition reports also include
`wrong_target` for real Squeezy target mismatches and `squeezy_only` for places
where Squeezy resolved a local-looking target but rust-analyzer returned no
definition. Reference probes include declarations because Squeezy's current
`reference_search` output includes declaration-like hits; this makes extra
same-name lexical matches visible as false positives instead of hiding the
precision loss.

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
| ripgrep | 1,000 | 468 ms | 5,284 ms | 50 / 15,190 | 8 | 2 | 13 | 2 | 0 | 50 / 3,276 | 210 | 4,424 | 0 |
| fd | 1,000 | 78 ms | 3,753 ms | 50 / 2,122 | 12 | 2 | 12 | 2 | 0 | 50 / 389 | 183 | 495 | 8 |
| bat | 1,000 | 329 ms | failed locally | 50 / 7,082 | 0 | 8 | 0 | 8 | 0 | 50 / 972 | 0 | 538 | 0 |
| serde | 1,000 | 430 ms | 7,539 ms | 50 / 9,540 | 4 | 1 | 13 | 1 | 0 | 50 / 2,804 | 146 | 5,087 | 9 |
| tokio | 1,000 | 1,451 ms | 4,834 ms | 50 / 40,098 | 0 | 6 | 6 | 6 | 0 | 50 / 8,787 | 27 | 15,073 | 0 |

The navigation FP/FN numbers are intentionally not zero. They show the main
accuracy losses Squeezy currently accepts for speed: lexical reference search
returns many same-name extras, workspace-only indexing misses stdlib/external
call targets, and method resolution can pick the wrong unique local method for
common names such as `get` and `push`.

## CI

`.github/workflows/semantic-graph-benchmark.yml` runs the smoke benchmark on PRs
and pushes. `workflow_dispatch` with `tier=full` clones ripgrep, fd, bat, tokio,
and serde, runs 5,000 deterministic mixed-workload scenarios per repo, and
writes timing, symbol accuracy, and rust-analyzer LSP navigation accuracy
summaries to the GitHub Actions step summary.
