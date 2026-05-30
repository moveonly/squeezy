# Eval Harness (`squeezy-eval`)

`squeezy-eval` lets an external operator — a human or an agent — drive the
real `squeezy-agent` loop against a target workspace, capture every event
the run produces, surface common regressions automatically, and (optionally)
have an LLM triage the result into draft tickets.

It is a peer to [`squeezy-harness`](./VALIDATION_HARNESS.md), not a
replacement. Harness is deterministic + mock-traced for CI; eval is live-agent
for exploratory QA, regression diffing, and triage.

## Table of contents

- [When to use which tool](#when-to-use-which-tool)
- [Quick start](#quick-start)
- [How an agent should use this](#how-an-agent-should-use-this)
- [Scenario file reference](#scenario-file-reference)
  - [Workspace](#workspace)
  - [Squeezy overlay](#squeezy-overlay)
  - [Steps: prompts and actions](#steps-prompts-and-actions)
  - [Expect (soft checks)](#expect-soft-checks)
  - [Triage](#triage)
  - [Mock provider](#mock-provider)
- [Per-run output layout](#per-run-output-layout)
  - [trace.jsonl schema](#tracejsonl-schema)
  - [frames.jsonl schema](#framesjsonl-schema)
  - [findings.jsonl + rule reference](#findingsjsonl--rule-reference)
  - [run.json](#runjson)
  - [tickets/](#tickets)
- [CLI reference](#cli-reference)
- [Recipes](#recipes)
- [Cost and token budgeting](#cost-and-token-budgeting)
- [Troubleshooting](#troubleshooting)
- [Limits and non-goals](#limits-and-non-goals)

---

## When to use which tool

| Need | Use |
|---|---|
| Single-turn correctness fixture, deterministic, runs in CI without API keys | [`squeezy-harness`](./VALIDATION_HARNESS.md) |
| Multi-turn scenario against a real agent + real workspace, find regressions | `squeezy-eval run` |
| Multi-turn scenario, no provider keys, just exercise the harness itself | `squeezy-eval run` with `[squeezy] provider = "mock"` |
| Compare today's run to yesterday's | `squeezy-eval diff <a> <b>` |
| Gate CI on a directory full of scenarios | `squeezy-eval check <dir>` |

---

## Quick start

### Fully offline (no keys, no network)

```sh
cargo run -p squeezy-eval -- run crates/squeezy-eval/fixtures/scenarios/mock-smoke.toml --no-triage
```

This exercises the harness end-to-end — workspace provisioning, scenario
parsing, the real agent loop, capture, frames, auto-findings, ticket
emission — using the built-in scripted `MockProvider`.

### Live, cheap, against the squeezy repo itself

```sh
source ~/.env.sh   # provides OPENAI_API_KEY
cargo run -p squeezy-eval -- run crates/squeezy-eval/fixtures/scenarios/live-openai-smoke.toml
```

A 1-turn smoke against `gpt-5.4-mini` (the cheapest model in the squeezy
registry). Cost is reported on the final summary line.

### Bug-hunting against the squeezy repo

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run crates/squeezy-eval/fixtures/scenarios/bug-probe-v2.toml
```

This is the canonical "probe a real repo with triage on" workflow.

---

## How an agent should use this

If you are an agent given a task like "find bugs in squeezy", "verify
this fix", or "regress this behavior", this is the playbook:

1. **Pick or author a scenario.**
   - For broad bug-hunting: copy `bug-probe-v2.toml` and tighten it
     around the surface you want to probe (e.g. only edit-flow prompts;
     drop `triage.focus` to your area).
   - For targeted regression of a fix: copy `find-and-fix.toml` shape;
     pin `triage.focus` to the bug ID you're verifying.
   - For offline plumbing checks: copy `mock-smoke.toml`; never hits an
     API.

2. **Provision a sandboxed workspace.** Set
   ```toml
   [workspace]
   local = "."
   snapshot = true
   ```
   so the agent reads a per-run `git worktree` of the source tree and
   any edits it attempts never touch your live checkout.

3. **Run it.**
   ```sh
   cargo run -p squeezy-eval -- run path/to/scenario.toml
   ```
   The end-of-run line prints the run directory + cost. You should
   never spend more than a few cents per probe against `gpt-5.4-mini`.

4. **Read the artifacts in this order:**
   1. **`run.json`** — totals, manifest, finding count. One-line health check.
   2. **`findings.jsonl`** — auto-detected regressions. If non-empty,
      this is where the bug is.
   3. **`frames.jsonl`** — one record per turn. Carries the assembled
      assistant text, per-tool-call breadcrumbs (with arg previews and
      sha256), token totals, and the per-turn cost. Most "what went
      wrong" answers are visible here.
   4. **`trace.jsonl`** — the full event stream. Only when you need
      the exact sequence (e.g. to confirm which event came before a
      failure).
   5. **`tickets/`** — markdown + JSON drafts when triage is enabled
      or when auto-findings fired.

5. **Compare against a baseline.**
   If you're verifying a fix:
   ```sh
   squeezy-eval diff target/eval/<old-run> target/eval/<new-run>
   ```
   The output shows totals delta, per-turn tool-call set diff, frame
   text diff, and a `findings delta` block listing new vs. resolved
   `rule_id`s.

6. **Write up the bug.** Cite specific evidence from the artifacts —
   `frames.jsonl` line N, `trace.jsonl` seq M — so the next agent can
   re-run the exact scenario and reproduce.

**Antipatterns:**
- Running without `snapshot = true` against your own checkout. The agent
  will see your WIP edits and may try to write back to your source tree.
- Designing scenarios that need many turns just to set up. Keep probes
  small and focused — 1–4 prompts is plenty. Cost scales with input
  tokens per turn, and input tokens balloon fast.
- Reading `trace.jsonl` before checking `findings.jsonl`. The rules
  exist specifically so you don't have to grep the trace by hand.

---

## Scenario file reference

A scenario is a TOML file with five top-level sections: `workspace`,
`squeezy`, `steps`, `expect`, `triage`. Plus the optional `mock` block
when the scripted provider is in use.

### Minimal example

```toml
id = "smoke"
title = "Smoke"

[workspace]
local = "."

[[steps]]
kind = "prompt"
text = "Which file defines `Agent::start_turn`?"

[expect]
final_text_contains = ["squeezy-agent"]
```

### Workspace

Exactly one source. Choose:

```toml
[workspace]
local = "/path/to/repo"          # plain local path
```

```toml
[workspace]
local = "."
snapshot = true                  # per-run git worktree of HEAD (recommended)
# snapshot_ref = "HEAD"          # optional, can be any rev
```

```toml
[workspace.github]
repo = "owner/name"              # shallow clone of a remote
sha  = "deadbeef1234"
```

`snapshot = true` falls back to an ignore-respecting tree copy when the
source is not a git repo. The scratch directory is cleaned up at end of
run via a `Drop` guard (worktrees use `git worktree remove --force`).

### Squeezy overlay

Pinned knobs for the resolved `AppConfig`. All fields optional:

```toml
[squeezy]
provider = "openai"               # or "anthropic" | "google" | "azure_openai" | "bedrock" | "ollama" | "mock"
model = "gpt-5.4-mini"
reasoning_effort = "low"          # optional: low | medium | high | xhigh
tool_choice = "required"          # optional: auto | required | none
mode = "build"                    # "plan" | "build"
permission_mode = "ask"           # "allow" | "ask" | "deny"; applied to edit/shell/web/mcp
instructions = "..."              # optional system instructions override
max_output_tokens = 1024
```

Setting `provider = "mock"` activates the built-in scripted provider; see
[Mock provider](#mock-provider).

`provider = "<preset>"` for any other registered preset (`openai`,
`anthropic`, `portkey`, `openrouter`, ...) now threads through
`AppConfig::from_env_and_settings_with_provider`, so the run's full
ProviderConfig (base URL, `api_key_env`, transport) is resolved against
that preset. You still need the matching API-key env var exported
(`OPENAI_API_KEY`, `PORTKEY_API_KEY`, etc.) — the scenario references
the provider by name only.

### Steps: prompts and actions

`steps` is an ordered array. Two kinds: `prompt` and `action`.

#### `prompt` step

```toml
[[steps]]
kind = "prompt"
text = "Which file defines make_widget?"
wait_for = "turn_completed"      # default; see below
```

`wait_for` controls when the driver moves on:

| `wait_for` | Meaning |
|---|---|
| `"turn_completed"` (default) | wait for `AgentEvent::Completed`/`Failed`/`Cancelled` |
| `{ kind = "tool_call", tool = "grep" }` | record a signal event when this tool fires; do **not** cancel the turn; concurrent dispatch lives on `when.on_tool` actions |
| `{ kind = "text_contains", text = "compiles" }` | cancel the turn the moment that substring appears in the assistant stream |

#### `action` step

Actions are imperative side-steps. The driver dispatches them between
prompts (or, when `when` is set, during a turn as soon as the trigger
matches).

| `action` | Required fields | Purpose |
|---|---|---|
| `approve` | optional `match.tool = "<name>"` | reply Approved to any matching `ApprovalRequested` |
| `deny` | optional `match.tool`, optional `reason` | reply Denied |
| `slash_command` | `command = "/compact"` | run an agent slash command (see below) |
| `edit_file` | `path` + either `content = "..."` or `replace = { find, with }` | mutate a workspace file mid-run |
| `wait_seconds` | `seconds = 5` | sleep |
| `cancel_turn` | — | cancel the most-recently-started turn |
| `assert` | `check = { kind = "text_contains" \| "max_tool_calls", ... }` | soft assertion; failure becomes a finding |
| `inject_user_text` | `text = "..."` | append a user message to the conversation transcript without starting a turn |

Every action accepts an optional `when` predicate. When `when` is set,
the action is queued and fires only when the trigger is observed:

```toml
[[steps]]
kind = "action"
action = "inject_user_text"
text = "Actually focus on X instead."

[steps.when]
on_tool = "grep"                  # fire mid-stream when grep is requested
# on_event = "tool_call_started" # alternative
```

#### `slash_command` coverage

Slash commands dispatch through `Agent::dispatch_command` (typed) via the
`Agent::dispatch_command_raw` shim. Every entry in
`squeezy-tui`'s `SLASH_COMMANDS` table is reachable; commands whose
behaviour lives entirely in the TUI renderer (overlays, transcript pushes,
clipboard, `/diff` snapshot) land as `DispatchOutcome::TuiOnly { command }`
so eval traces still observe the typed entry point. Unknown heads land as
`DispatchOutcome::Unsupported`, which surfaces as the
`unsupported_slash_command` auto-finding so triage flags missing
automation rather than silently no-op.

### Expect (soft checks)

Failures here produce findings, not aborts.

```toml
[expect]
final_text_contains = ["build_widget"]   # require text in the final turn's assistant output
max_wall_clock_seconds = 120
max_input_tokens = 50000
max_tools_per_turn = 8                    # tunes the high_tool_burst auto-finding
no_tool_errors = true
```

Soft expectations are converted into findings with stable
`rule_id`s (`expect_final_text_contains`, `expect_wall_clock`,
`expect_input_tokens`, `expect_no_tool_errors`) so they show up
alongside the heuristics in `findings.jsonl` and `tickets/`.

### Triage

```toml
[triage]
enabled = true
model = "gpt-5.4-mini"            # optional; defaults to scenario.squeezy.model
provider = "openai"               # advisory; uses the run's provider unless overridden
focus = "test /compact behavior"  # one-line steer
extra_prompt = "Be terse."        # arbitrary extra text appended to instructions
```

When enabled, after the run completes the triage step sends a tail of
`trace.jsonl` + `frames.jsonl` to the configured model with a strict
JSON-only prompt asking for ticket drafts. The drafts land alongside
auto-finding tickets in `tickets/`.

### Mock provider

For offline runs and CI:

```toml
[squeezy]
provider = "mock"

[mock]
default_text = "(mock fallback for unscripted turns)"

[[mock.turns]]
text = "src/lib.rs defines make_widget."
input_tokens = 42
output_tokens = 7

[[mock.turns]]
text = "Yes — tests/widget.rs covers it."
tool_calls = [{ name = "grep", arguments = { pattern = "make_widget" } }]
```

Each `mock.turns` entry can also carry `tool_calls` to exercise the
agent's tool / approval path. The mock fools only the LLM layer; the
rest of the agent loop (workspace tools, exploration planner,
redaction, telemetry) runs for real, which is exactly what makes the
mock useful for self-testing the harness.

---

## Per-run output layout

```
target/eval/<scenario-id>-<unix-ms>/
├── run.json
├── trace.jsonl
├── frames.jsonl
├── findings.jsonl
└── tickets/
    ├── 01-<slug>.md
    ├── 01-<slug>.json
    ├── 02-...
    └── session-bundle.tar.gz       # redacted SessionStore::build_bug_report
```

### trace.jsonl schema

One JSON object per line. `schema_version = 2`. Common envelope:

```json
{
  "schema_version": 2,
  "ts_unix_ms": 0,
  "sequence": 0,
  "turn_id": "TurnId(1)",
  "kind": "<variant>"
}
```

Variants (`kind`):

| `kind` | Payload | Emitted when |
|---|---|---|
| `user_message` | `text` | prompt step sends a user message |
| `turn_started` | — | agent turn starts |
| `turn_completed` | `metrics`, `cost` | agent turn ends successfully |
| `turn_failed` | `error` | agent turn errors |
| `turn_cancelled` | — | agent turn cancelled |
| `assistant_delta` | `delta` | streaming assistant token chunk |
| `tool_call_queued` | `call` | agent queues a tool call |
| `tool_call_started` | `call` | tool actually runs |
| `tool_call_completed` | `result` | tool finishes (status `Success` / `Error` / `Cancelled`) |
| `approval` | `request`, `decision` | ApprovalRequested + driver's response |
| `context_compacted` | `report` | /compact ran |
| `task_state_updated` | `snapshot` | agent reports a task-state update |
| `subagent_event` | `event` | subagent started / completed / failed |
| `slash_command` | `command` | a slash_command action fired |
| `action_step` | `action`, `status` | any other action |
| `snapshot` | `snapshot_kind`, `payload` | misc snapshots (mcp_status, jobs, cost_warning, ai_reviewer_tripped) |
| `perf_sample` | `label`, `ms` | reserved for future per-call timing |
| `finding` | `rule_id`, `severity`, `summary` | embedded finding for downstream tooling |

### frames.jsonl schema

One record per assistant turn:

```json
{
  "turn_id": "TurnId(1)",
  "prompt": "Which file defines make_widget?",
  "assistant_text": "src/lib.rs defines make_widget.",
  "tool_calls": [
    {
      "name": "grep",
      "args_preview": "{\"pattern\":\"make_widget\"}",
      "args_sha256": "abc123...",
      "status": "success"
    }
  ],
  "tool_errors": [],
  "elapsed_ms": 1677,
  "input_tokens": 14314,
  "output_tokens": 92,
  "cost_micro_usd": 17880,
  "cost_display": "$0.0179",
  "styled_lines": [{ "spans": [{ "text": "src/lib.rs", "fg": null, "bg": null, "modifiers": [] }] }],
  "ansi": "src/lib.rs defines make_widget.\n",
  "finish": "completed",
  "finish_reason": "stop",
  "reasoning_only_stop": false,
  "dropped_tool_calls": 0
}
```

- `tool_calls[i].args_sha256` is the **same hash the auto-findings
  `duplicate_tool_call` rule keys on**. If two entries in the same
  frame have the same sha, you found the bug already.
- `styled_lines` flattens ratatui `Line`/`Span` into plain JSON; ratatui
  types do not leak into the schema.
- `ansi` is a re-rendering you can `cat` into a terminal.
- `cost_micro_usd` is the result of `squeezy_llm::estimate_cost`;
  `0` means no pricing entry for the model.
- `finish_reason` is the provider-reported terminal reason
  (`stop` / `length` / `tool_calls` / `content_filter` for
  Chat-Completions; `end_turn` / `max_tokens` / `tool_use` for
  Anthropic). `null` when the provider didn't surface one (synthetic
  end on truncated stream, helper paths, replay reconstruction).
- `reasoning_only_stop` is `true` when the final round was the
  Qwen3-style "thinks but emits nothing" pattern (stop + reasoning text
  but no content or tool call). The
  `stop_with_intent_text_no_tool_call` finding rule keys off the
  related but distinct content-emitted-but-no-tool case.
- `dropped_tool_calls` is the count of chat-completions tool-call
  frames the provider dropped this turn because the stream cut before
  a function name arrived. Always 0 for native OpenAI / Anthropic /
  Google / Bedrock / Ollama. A non-zero value is a likely contributor
  to the "I'll do X then stop" Qwen pattern — the model thinks it
  emitted the call but the wire frame was incomplete.

### findings.jsonl + rule reference

One JSON object per finding. Stable `rule_id` keys make findings
diffable across runs.

| `rule_id` | Severity | Triggers when |
|---|---|---|
| `duplicate_tool_call` | major | same turn fires ≥ 2 tool calls with identical `args_sha256` |
| `repeated_turn_failure` | major | two consecutive `turn_failed` events with byte-equal error text |
| `stale_function_call_output` | critical | `turn_failed.error` matches `"No tool call found for function call output"` |
| `high_tool_burst` | minor | one turn fires > `expect.max_tools_per_turn` (default 10) tool calls |
| `unsupported_slash_command` | minor | any `action_step.status` starts with `unsupported_slash_command:` |
| `approval_unanswered` | major | an `Approval` event has `decision` starting with `denied_no_action` |
| `stop_with_intent_text_no_tool_call` | major | turn finished `finish_reason=stop`, zero tool calls, assistant text contains an intent phrase like `"let me X"` / `"i'll Y"` with an action verb — the canonical Qwen3 chatty-preamble-then-stop pattern |
| `expect_wall_clock` | minor | wall clock > `expect.max_wall_clock_seconds` |
| `expect_input_tokens` | minor | total input tokens > `expect.max_input_tokens` |
| `expect_final_text_contains` | minor | the last turn's assistant_text missing a required substring |
| `expect_no_tool_errors` | minor | any `Error`/`Cancelled` `tool_call_completed` and `expect.no_tool_errors = true` |
| `expect_finish_reason` | major | any completed turn matches `expect.finish_reason_not` — literal match against the provider's `finish_reason`, or the sentinel `"stop_no_action"` (stop + zero tool calls) |

Adding a new rule is a single file change in
`crates/squeezy-eval/src/findings.rs`: implement the `Rule` trait,
register it in `default_rules()`. The trace context carries
`tool_calls_by_turn`, `turn_failures`, `action_steps`, `approvals`, and
totals — most rules are 10–20 lines.

### run.json

```json
{
  "schema_version": 2,
  "scenario": { "id": "...", "title": "...", "path": "..." },
  "workspace": { "kind": "local|snapshot|github" },
  "provider": "openai",
  "model": "gpt-5.4-mini",
  "totals": {
    "trace_events": 57,
    "frames": 3,
    "findings": 1,
    "cost_micro_usd": 22900,
    "cost_display": "$0.0229"
  },
  "per_turn_costs": [{ "turn_id": "TurnId(1)", "cost_micro_usd": 18000 }],
  "findings": ["[approval_unanswered] ApprovalRequested arrived..."],
  "squeezy_version": "0.1.0"
}
```

### tickets/

One markdown + one JSON per ticket. Schema (`*.json`):

```json
{
  "id": "duplicate_tool_call",
  "title": "[duplicate_tool_call] Turn TurnId(1): grep fired 3 times...",
  "severity": "major",
  "category": "perf",
  "summary": "...",
  "repro": "Run scenario `bug-probe`.",
  "evidence": [{ "trace_event": 142, "frame": 0 }],
  "suggested_fix": "..."
}
```

`session-bundle.tar.gz` is the output of
`SessionStore::build_bug_report` — a redacted reproducible bundle
including `version.json`, `config.toml`, `repo_profile.json`, and the
session's `events.jsonl`. Every ticket markdown body links to it under
`## Bundle`.

---

## CLI reference

```
squeezy-eval run <scenario.toml> [--workspace-override <path>]
                                 [--no-triage]
                                 [--emit github --gh-repo owner/name]
                                 [--out <dir>]
                                 [--quiet]

squeezy-eval list [<dir>]                       # ls bundled or directory scenarios
squeezy-eval replay <trace.jsonl>               # one-line summary of a recorded trace
squeezy-eval view <run-dir>                     # chronological markdown transcript of a run
squeezy-eval diff <run-a> <run-b> [--format markdown|json]
squeezy-eval check <dir> [--fail-on findings,expectations,errors]
                          [--junit <path>]
                          [--out <dir>]
```

Flags:

- `--no-triage`: skip the LLM triage pass even if the scenario enables it.
- `--workspace-override <path>`: replace the scenario's `[workspace]`
  with a plain local path (no snapshot). Useful for one-off debugging.
- `--emit github --gh-repo owner/name`: also file each ticket as a
  GitHub issue via `gh issue create`. Requires `gh auth`. Failure is
  non-fatal; disk artifacts remain the source of truth.
- `--out <dir>`: change the per-run scratch root (default `target/eval`).
- `--fail-on`: comma list. `expectations` = any `expect_*` rule fired,
  `findings` = any non-expect rule fired, `errors` = scenario errored
  out (provider not configured, IO error, etc.). Default is
  `expectations,errors`.
- `--junit <path>`: write a JUnit XML summary; one `<testcase>` per scenario.
- `--quiet` (on `run`): suppress the default live narration. Without it, `squeezy-eval run` streams squeezy's activity to stdout as it happens — step boundaries, tool calls (`🔧 name(args)` + `↳ ✅/❌ status (bytes)`), the assistant's streaming text rendered inline, approvals, slash commands, and findings the moment each rule fires. Use `--quiet` for CI or when piping output into a pager. `check` always runs quietly because per-scenario PASS/FAIL is the right granularity for batch mode.

### `view` output

`squeezy-eval view <run-dir>` prints a markdown transcript that interleaves
user prompts, assistant deltas, tool calls (with arg preview + status +
byte counts), approvals, slash commands, findings, and per-turn cost so
you can follow exactly what the agent did without parsing `trace.jsonl`
by hand. Suitable for piping into a terminal pager or pasting into a PR
comment.

---

## Recipes

**Verify a fix landed.** Stash the pre-fix run, apply the fix, re-run,
diff:

```sh
mv target/eval/<scenario>-<ts> target/eval/baseline
# apply fix
cargo run -p squeezy-eval -- run path/to/scenario.toml
squeezy-eval diff target/eval/baseline target/eval/<scenario>-<new-ts>
```

The `Findings delta` block shows `✅ resolved: <rule_id>` for each fix.

**CI gate on every scenario in a directory.**

```sh
squeezy-eval check crates/squeezy-eval/fixtures/scenarios --junit target/junit.xml
```

Exit non-zero if any scenario violates `--fail-on`.

**Probe a single hypothesis quickly.** One-prompt scenario with the
mock provider so iteration is free:

```toml
id = "hyp-x"
title = "Does feature X behave as expected"

[workspace]
local = "."

[squeezy]
provider = "mock"

[[steps]]
kind = "prompt"
text = "Use X to do Y."

[mock]
[[mock.turns]]
text = "(scripted answer)"
tool_calls = [{ name = "<tool you want to exercise>", arguments = {} }]
```

**Triage with a tight focus** when probing a known-flaky area:

```toml
[triage]
enabled = true
focus = "transcript-state regressions only — ignore stylistic findings"
```

---

## Cost and token budgeting

- Every turn against `gpt-5.4-mini` costs roughly `input_tokens *
  0.4¢/1M` + `output_tokens * 1.6¢/1M`. A trivial nav prompt against
  the squeezy repo lands around 14k–50k input tokens because the
  system prompt + repo profile is substantial.
- Set `max_input_tokens` on `[expect]` to catch runaway burns; the
  `expect_input_tokens` rule fires when crossed.
- The mock provider has zero cost and runs the *real* agent loop
  underneath — use it whenever the LLM's actual answer is not what you
  care about (you're testing the harness or the agent's tool-call
  pattern).
- The CLI's end-of-run line prints `cost: $X.XXXX`. The same number,
  plus per-turn breakdown, is in `run.json:totals` and per-frame in
  `frames.jsonl:cost_micro_usd`.

---

## Troubleshooting

| Symptom | Cause / remedy |
|---|---|
| `provider is not configured: missing OPENAI_API_KEY` | export the key (e.g. `source ~/.env.sh`), or set `[squeezy] provider = "mock"` for offline runs |
| `approval_unanswered` finding fires unexpectedly | tool name in your scenario's `match.tool` doesn't match what squeezy emits — check `trace.jsonl` for the actual `approval.request.tool` |
| `tool_calls: []` in a frame, but the trace has tool_call events | older bug, fixed: tool names now push on `ToolCallStarted` (with status filled later by `ToolCallCompleted`) |
| Triage produced no tickets | check `frames.jsonl` for the LLM's raw response in `assistant_text`; the triage prompt requires a strict JSON wrapper, fenced or otherwise — if the model added prose it gets dropped |
| Snapshot workspace had unexpected content | confirm `snapshot_ref` is what you wanted; `git worktree list` after a failure shows whether cleanup ran |
| `git worktree add` fails because the source has uncommitted changes | the worktree uses `--detach`, so committed state is required; commit (or use a non-snapshot local workspace) |

---

## Limits and non-goals
- **Slash-command coverage** is the subset that lives wholly inside
  `Agent` (compact / plan / build / cost / jobs / permissions). TUI-only
  commands (overlays, help text) return `Unsupported`.
- **`wait_for: tool_call` is signal-only.** Scenarios that want
  concurrent action dispatch attach `when.on_tool` to the action they
  want fired mid-stream.
- **No pixel-accurate TUI capture.** `styled_lines` + `ansi` is enough
  for presentation review; a `TestBackend` screen recorder is out of
  scope for now.
- **No record/replay of an eval trace through the agent.** That's a
  different problem from `squeezy-cli sessions replay`.
- **Single-process, single-machine.** No distributed runner. CI workers
  run scenarios serially via `squeezy-eval check`.
