# Eval Fixture Automation

Status: archived design sketch. The committed `squeezy-eval` CLI currently
ships `run`, `list`, `replay`, `view`, `diff`, and `check`. It does not ship
fixture-generation or matrix-orchestration subcommands, nor does the repository
contain the template or roster paths described in the older proposal.

Keep this file as design context for future eval-harness work. The current
fixture source of truth remains the checked-in scenario TOML files under
`crates/squeezy-eval/fixtures/scenarios/`, the schema in
`crates/squeezy-eval/src/scenario.rs`, and the CLI command surface in
`crates/squeezy-eval/src/main.rs`.

## Current Manual Workflow

1. Add or edit a scenario TOML under
   `crates/squeezy-eval/fixtures/scenarios/`.
2. Run it with `cargo run -p squeezy-eval -- run <scenario> --quiet`.
3. Inspect output with `view`, compare runs with `diff`, or batch-check
   scenarios with `check`.
4. Keep generated workspaces and captured run artifacts under `target/eval/`.

## Archived Proposal

The original idea was to generate many graph-navigation scenarios from a small
roster of repositories and public symbols. A future implementation could add:

- a deterministic generator that expands a roster into scenario TOML;
- a symbol picker that uses Squeezy's parser/graph to suggest public types;
- an optional LLM-assisted picker for libraries whose central symbols are not
  obvious from manifests;
- a matrix runner that wraps today's `check` and `diff` commands for nightly
  paid-provider sweeps.

Those remain proposed features. Do not cite `squeezy-eval generate`,
`squeezy-eval matrix`, `docs/internal/eval-roster.yaml`, or
`crates/squeezy-eval/templates/graph-nav.toml.tera` as existing interfaces until
the corresponding code and fixtures are added.
