# Eval Harness (`squeezy-eval`)

`squeezy-eval` is an agent-driven QA harness: it lets an external agent (or a
human) drive the real `squeezy-agent` loop against a target repository,
capture every event/perf/text frame the run produces, and (optionally) ask
an LLM to turn the trace into draft tickets.

It is a peer to [`squeezy-harness`](./VALIDATION_HARNESS.md), not a
replacement. Harness stays mock-trace deterministic for CI; eval is
live-agent for exploratory QA.

## Quick start

The fully offline path — no provider keys, no network — runs the
bundled `mock-smoke` scenario against the built-in scripted provider:

```sh
cargo run -p squeezy-eval -- run crates/squeezy-eval/fixtures/scenarios/mock-smoke.toml --no-triage
```

For live runs against a real provider (after `squeezy auth` is set up),
swap in `find-and-fix.toml` or any of your own scenarios:

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

## Per-run outputs

Each run writes to `target/eval/<scenario-id>-<ts>/`:

- `trace.jsonl` — structured event stream (one JSON object per line; schema_version=2).
- `frames.jsonl` — one frame per turn. Each frame carries the assembled assistant markdown, per-tool-call breadcrumbs (`name`, `args_preview`, `args_sha256`, `status`), elapsed wall clock, token totals, the per-turn USD cost, **and** a TUI-rendered view: `styled_lines` (structured `Line`/`Span` JSON with `fg`/`bg`/`modifiers`) plus `ansi` (an ANSI-escaped string you can `cat` into your terminal). Rendering goes through the same `squeezy_tui::render_markdown` pipeline the TUI uses, so palette and modifier regressions in the TUI surface in eval frames for free.
- `findings.jsonl` — auto-derived findings from the rule matcher (`duplicate_tool_call`, `repeated_turn_failure`, `stale_function_call_output`, `high_tool_burst`, `unsupported_slash_command`, `approval_unanswered`, plus `expect_*` rules promoted from soft expectations).
- `run.json` — manifest with totals (events, frames, findings, `cost_micro_usd`, per-turn cost breakdown) plus scenario / workspace / provider / model metadata.
- `tickets/` — markdown + JSON per ticket; when the session log is available a shared `tickets/session-bundle.tar.gz` is produced via `SessionStore::build_bug_report`, and each markdown body links to it under `## Bundle`.

## Sandboxed local workspaces

Set `snapshot = true` on a `[workspace]` `local = "..."` block (and optionally `snapshot_ref = "<ref>"`) to materialize a per-run snapshot before the agent runs. If the source is a git repo we use `git worktree add --detach`; otherwise we copy the tree respecting `.gitignore`. The scratch directory is cleaned up automatically. This keeps the agent off your in-progress edits.

```toml
[workspace]
local = "."
snapshot = true
# snapshot_ref = "HEAD"  # optional
```

## Triage focus

When triage is enabled, narrow the LLM's attention to one surface area to get sharper tickets:

```toml
[triage]
enabled = true
focus = "test /compact behavior"
# extra_prompt = "Be terse and only flag transcript-state bugs."
```

## CI mode

```sh
squeezy-eval check crates/squeezy-eval/fixtures/scenarios \
                   --fail-on expectations,errors \
                   --junit target/eval/junit.xml
```

Iterates every `*.toml` scenario in the directory. `--fail-on` accepts a comma list of `findings`, `expectations`, `errors`. Exits non-zero when any scenario violates the policy. JUnit XML output is optional.

## Diff

Compare two run directories:

```sh
squeezy-eval diff target/eval/<run-a> target/eval/<run-b>
```

Prints a markdown delta covering totals, per-turn tool-call set difference (added/removed `(name, args_sha256)` pairs), unified text diff of assistant frames, and findings delta (new / resolved rule ids). Pass `--format json` for a structured payload.

## Offline / mock provider

Set `[squeezy] provider = "mock"` to use the built-in scripted
provider. The scenario then declares a `[mock]` block with the
responses, one per agent turn:

```toml
[mock]
default_text = "(mock fallback for unscripted turns)"

[[mock.turns]]
text = "src/lib.rs defines make_widget."
input_tokens = 42
output_tokens = 7

[[mock.turns]]
text = "Yes — tests/widget.rs covers it."
```

Each `mock.turns` entry can also carry `tool_calls = [{ name, arguments }]`
to exercise the agent's tool / approval path. The mock fools only the
LLM layer; the rest of the agent loop (workspace tools, exploration
planner, redaction, telemetry) still runs for real against the
target workspace, which is exactly what makes this a useful
self-test.

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
