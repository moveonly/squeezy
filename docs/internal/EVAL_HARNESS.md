# Eval Harness (`squeezy-eval`)

`squeezy-eval` is an agent-driven QA harness: it lets an external agent (or a
human) drive the real `squeezy-agent` loop against a target repository,
capture every event/perf/text frame the run produces, and (optionally) ask
an LLM to turn the trace into draft tickets.

It is a peer to [`squeezy-harness`](./VALIDATION_HARNESS.md), not a
replacement. Harness stays mock-trace deterministic for CI; eval is
live-agent for exploratory QA.

## Quick start

```sh
cargo run -p squeezy-eval -- list crates/squeezy-eval/fixtures/scenarios
cargo run -p squeezy-eval -- run crates/squeezy-eval/fixtures/scenarios/find-and-fix.toml --no-triage
```

Each run writes to `target/eval/<scenario-id>-<ts>/`:

- `trace.jsonl` — one normalized event per line (user/turn/tool/approval/snapshot).
- `frames.jsonl` — one assembled assistant-text "frame" per turn, with
  perf and token totals — the closest thing to "what a TUI user would
  have seen".
- `run.json` — top-level manifest (scenario, workspace, totals, findings).
- `tickets/<NN>-<slug>.{md,json}` — ticket drafts (when triage runs or
  expectations fail).

Inspect a recorded run:

```sh
cargo run -p squeezy-eval -- replay target/eval/<run>/trace.jsonl
```

## Scenario authoring

A scenario is a TOML file with four sections:

1. **Workspace** — local path or GitHub @ SHA. GitHub clones into a
   tempdir under `target/eval/_workspaces/`.
2. **Squeezy overlay** — optional knobs (`model`, `mode`, `permission_mode`,
   `instructions`, `max_output_tokens`) layered onto the resolved
   `AppConfig`.
3. **Steps** — `prompt` and `action` steps in order. Actions include
   `approve`, `deny`, `slash_command`, `edit_file`, `wait_seconds`,
   `assert`, `cancel_turn`. Each action can carry a `when` predicate
   (`on_tool = "..."`) so it fires when a matching tool call appears
   during the next turn — that is the "customize when to execute"
   surface from the design brief.
4. **Expect** + **Triage** — soft post-run checks and an optional LLM
   triage pass that produces tickets.

See [`fixtures/scenarios/find-and-fix.toml`](../../crates/squeezy-eval/fixtures/scenarios/find-and-fix.toml)
and [`perf-budget.toml`](../../crates/squeezy-eval/fixtures/scenarios/perf-budget.toml)
for examples.

## Tickets

When triage runs (or any expectation fails) eval writes one
markdown + one JSON per finding under `tickets/`. With
`--emit github --gh-repo owner/name` it also shells out to `gh issue
create`. Local artifacts are the source of truth; failing to open a GH
issue does not abort the run.

## Limits in the first cut

- The `provider` overlay is currently advisory. Switching providers
  requires re-resolving the full provider config, which we will plumb
  through `AppConfig::from_env_and_settings_with_provider` in a follow-up.
- `slash_command` understands `/compact`, `/plan`, `/build`. Other
  commands are recorded as `unsupported_slash_command:*` so triage can
  flag missing automation rather than silently no-op.
- Workspace `edit_file` resolves relative paths against the process
  cwd; if a scenario uses workspace-relative paths inside a GitHub-
  provisioned tempdir, supply absolute paths or run eval from the
  workspace root.
- TUI capture is text-frame only (assistant deltas streamed per turn).
  Structured ratatui buffer snapshots are out of scope.
