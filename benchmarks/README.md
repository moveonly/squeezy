# Semantic Graph Benchmarks

Benchmark documentation lives in
[`docs/internal/BENCHMARKS.md`](../docs/internal/BENCHMARKS.md).

This directory contains benchmark fixtures, specs, corpus configuration, and the
`squeezy-graph-bench` CLI. The benchmark crate exercises Squeezy's production
tree-sitter parsers and graph builders, then compares fixture queries and
language-specific oracle output where an oracle is available. User-facing
language coverage is summarized in
[`crates/squeezy-skills/external-docs/LANGUAGES.md`](../crates/squeezy-skills/external-docs/LANGUAGES.md).

Current layout:

- `fixtures/<language>/semantic-cases/`: deterministic smoke fixtures.
- `specs/*-smoke-queries.json`: expected query results for fixture gates.
- `corpus.json`: pinned smoke/full corpus manifest.
- `oracle/`: language-specific oracle wrappers checked into the repo.
- `oracle-helpers/`: helper projects for oracles that need their own build
  metadata.
- `baselines/` and `heuristic-history/`: checked benchmark history for
  language follow-up work.
- `scripts/`: summary and documentation consistency helpers.

Use `cargo run --release --manifest-path benchmarks/squeezy-graph-bench/Cargo.toml -- --list-languages`
and `--list-oracles` for the current runnable surface.
