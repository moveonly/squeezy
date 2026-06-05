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
  - [findings.jsonl + rule reference](#findingsjsonl-rule-reference)
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

A 1-turn smoke against `gpt-5.4-mini`, the low-cost OpenAI model pinned by the
fixture. Cost is reported on the final summary line.

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
   The end-of-run line prints the run directory + cost. Keep live probes
   tightly scoped and use the current small-fast or fixture-pinned model when
   the task does not need the parent model.

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
batch_tool_calls_hint = true       # optional: nudge independent read-only lookups into one batch
instructions = "..."              # optional system instructions override
cache_root = ".squeezy/eval-cache" # optional; relative paths stay workspace-relative
max_output_tokens = 1024
max_tool_calls_per_turn = 16
max_tool_bytes_read_per_turn = 10485760
max_session_cost_usd_micros = 100000
show_reasoning_usage = false
checkpoints_enabled = true
excluded_tools = ["repo_map", "decl_search"]
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

`excluded_tools` pushes names into `AppConfig.tools.excluded`; the
graph-vs-no-graph benchmarks use it to hide semantic-navigation tools
from the no-graph half of the pair.

Scenarios that exercise `/undo`, `/checkpoint`, `/revert-turn`, or
other checkpoint-backed git/VCS surfaces need Squeezy's checkpoint tracking
enabled. Either set
`[squeezy] checkpoints_enabled = true` in the scenario overlay (the
recommended path — keeps the scenario self-contained) or export
`SQUEEZY_CHECKPOINTS_ENABLED=1` in the shell before running. Without
one of those, edits do not produce checkpoint metadata and `/undo`
becomes a no-op.

### TUI capture

`[tui_capture]` enables the headless TUI render path. With `enabled = true`,
the driver writes `frames_tui.jsonl` and `replay.tui`; with
`drive_tui = true`, prompts, slash commands, key events, modal state, and
TUI assertions run through a live `TuiHarness` instead of only the agent-side
dispatch path.

```toml
[tui_capture]
enabled = true
drive_tui = true
width = 160
height = 48
palette_tone = "dark"             # "dark" (default) or "light"
```

Use `drive_tui = true` for `send_key`, `send_keys`, `tui_*`,
`modal_active`, and `config_screen_section` assertions. Keep it off for
pure agent-loop probes where a live TUI would add unnecessary state.

### Steps: prompts and actions

`steps` is an ordered array. Two kinds: `prompt` and `action`.

The current scenario schema also supports environment variables, platform pins,
fixture skills, MCP server injection, MCP elicitation injection, user-input
responses, attachments, mode switching, apply-diff actions, TUI key driving,
session-id capture, TUI frame assertions, per-turn token caps, and
dropped-tool-call expectations. Use this section as the operator overview and
`crates/squeezy-eval/src/scenario.rs` as the exhaustive field reference.

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
| `{ tool_call = { tool = "grep" } }` | record a signal event when this tool fires; do **not** cancel the turn; concurrent dispatch lives on `when.on_tool` actions |
| `{ text_contains = { text = "compiles" } }` | cancel the turn the moment that substring appears in the assistant stream |

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
| `apply_diff` | `path`, `unified_diff` | apply a unified diff to a workspace file via `git apply` |
| `wait_seconds` | `seconds = 5` | sleep |
| `cancel_turn` | — | cancel the most-recently-started turn |
| `assert` | `check = { kind = "text_contains" \| "max_tool_calls", ... }` | soft assertion; failure becomes a finding |
| `inject_user_text` | `text = "..."` | append a user message to the conversation transcript without starting a turn |
| `respond_elicitation` | `decision = { action = "accept" \| "decline" \| "cancel", ... }` | answer a real `McpElicitationRequested` event |
| `inject_mcp_elicitation` | `request = { server, message, kind = "form" \| "url", ... }` | synthesize an MCP elicitation into the live TUI modal; requires `drive_tui = true` |
| `respond_user_input` | `decision = { action = "choice" \| "freeform" \| "cancel", ... }` | answer a `request_user_input` prompt from plan mode |
| `switch_mode` | `mode = "plan" \| "build"` | switch session mode through the slash-command path |
| `attach_file` | `path` | attach a workspace file as context |
| `detach_attachment` | `id` | detach a prior attachment |
| `send_key` | `key = "Ctrl+O"` | send one key to the live TUI; requires `drive_tui = true` |
| `send_keys` | `keys = ["Down", "Enter"]`, optional `delay_ms` | send a key sequence to the live TUI |
| `capture_session_id` | `var = "name"` | capture the current session id for later `${name}` substitution in slash commands |

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

Slash commands dispatch through the typed `DispatchCommand` parser. With
`[tui_capture] drive_tui = true`, the driver routes the command through
the live `TuiHarness`, so visual commands such as overlays, model/config
screens, help, `/diff`, and TUI status updates take effect. Without
`drive_tui`, the driver calls `Agent::dispatch_command_raw`; pure agent-state
commands return structured statuses, and visual-only commands land as
`DispatchOutcome::TuiOnly { command }`. Unknown heads land as
`DispatchOutcome::Unsupported`, which surfaces as the
`unsupported_slash_command` auto-finding.

#### Assertion checks

`action = "assert"` supports:

| `check.kind` | Purpose |
|---|---|
| `text_contains` | latest assembled assistant output contains text |
| `max_tool_calls` | run has observed at most `max` tool calls |
| `tool_call_with_args` | a tool call with matching argument text fired |
| `finding_fired` | deferred assertion that a rule id appears in findings |
| `stop_reason` | latest stop reason equals or is not in a set |
| `task_state_contains` | task-state snapshots contain a step/blocker substring |
| `tui_status_contains` | live TUI status line contains text |
| `tui_transcript_entry` | transcript entry kind/collapsed state matches |
| `tui_frame_contains` / `tui_frame_does_not_contain` | latest rendered frame text includes/excludes text |
| `tui_cell_luminance_le` | rendered cell foreground/background stays under a luminance cap |
| `modal_active` | the current foreground modal is named, or no modal is active |
| `config_screen_section` | config modal focus is on a specific section slug |
| `action_step_status_contains` | a slash/action status contains a substring |

### Expect (soft checks)

Failures here produce findings, not aborts.

```toml
[expect]
final_text_contains = ["build_widget"]   # require text in the final turn's assistant output
max_wall_clock_seconds = 120
max_input_tokens = 50000
max_input_tokens_per_turn = 20000
max_tools_per_turn = 8                    # tunes the high_tool_burst auto-finding
no_tool_errors = true
finish_reason_not = ["length", "stop_no_action"]
max_dropped_tool_calls = 0
event_timeout_seconds = 120
```

Soft expectations are converted into findings with stable
`rule_id`s (`expect_final_text_contains`, `expect_wall_clock`,
`expect_input_tokens`, `expect_input_tokens_per_turn`,
`expect_no_tool_errors`, `expect_finish_reason`,
`expect_dropped_tool_calls`) so they show up alongside the heuristics
in `findings.jsonl` and `tickets/`.

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

### Fixture skills

Inline `SKILL.md` bundles are written into
`<workspace>/.squeezy/skills/<dir>/SKILL.md` after the workspace
snapshot is provisioned, so the catalog discovers them like any
user-authored skill:

```toml
[[fixture_skills]]
dir = "fixture-echo"
content = """
---
name: fixture-echo
description: Eval-only fixture skill.
triggers: [fixture echo trigger]
---
# Body
...
"""
```

The `dir` value must be a simple directory name (no `/`, no `.`) and
should match the frontmatter `name:` — the skill catalog rejects
mismatched names. See `fixtures/scenarios/skills-fixture-*.toml`.

### Inline MCP servers

`[mcp.servers.<name>]` entries are merged into
`AppConfig.mcp_servers` after the standard config load, so they
participate in tool discovery exactly like a user-defined server. The
eval driver pre-warms the MCP cache before the first prompt so the
very first turn can issue `mcp__*` tool calls without racing the
production background refresh.

```toml
[mcp.servers.bench]
transport = "stdio"           # or "http" | "sse"
command = "bundled:fake-mcp"  # sentinel resolved to the eval crate's bundled binary
args = []
enabled = true                # default true
timeout_ms = 10000            # bound the bring-up so a wedged binary fails fast
```

`bundled:fake-mcp` is resolved to the sibling `squeezy-fake-mcp`
binary that `cargo build -p squeezy-eval` produces. It speaks the
2025-03-26 MCP protocol over stdio and exposes three tools:

| tool | behavior |
|---|---|
| `echo` | echoes `message` back as text content |
| `add` | returns `a + b` as text content |
| `fail` | always returns `isError: true` |

See `fixtures/scenarios/mcp-fake-*.toml` for offline MCP scenarios.

---

## Per-run output layout

```
target/eval/<scenario-id>-<unix-ms>/
├── run.json
├── trace.jsonl
├── frames.jsonl
├── frames_tui.jsonl                # only when [tui_capture] enabled = true
├── replay.tui                      # only when [tui_capture] enabled = true
├── findings.jsonl
└── tickets/
    ├── 01-<slug>.md
    ├── 01-<slug>.json
    ├── 02-...
    └── session-bundle.tar.gz       # redacted SessionStore::build_bug_report
```

### trace.jsonl schema

One JSON object per line. `schema_version = 3`. Common envelope:

```json
{
  "schema_version": 3,
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
| `reasoning_delta` | `delta` | streaming reasoning token chunk |
| `reasoning_segment` | `display_text`, `payload` | completed structured reasoning segment |
| `shell_sandbox_degraded` | `backend`, `fallback_count` | shell sandbox degraded to a best-effort backend |
| `tool_call_queued` | `call` | agent queues a tool call |
| `tool_call_started` | `call`, `origin` | tool actually runs |
| `tool_call_completed` | `result` | tool finishes (status `Success` / `Error` / `Cancelled`) |
| `tool_progress` | progress payload | a running tool reports progress |
| `approval` | `request`, `decision` | ApprovalRequested + driver's response |
| `context_compacted` | `report` | /compact ran |
| `task_state_updated` | `snapshot` | agent reports a task-state update |
| `subagent_event` | `event` | subagent started / completed / failed |
| `mcp_server_event` | server event payload | MCP server discovery/status changed |
| `job_event` | job event payload | background job event reached the TUI |
| `cost_update` | cost payload | cost broker emitted an update |
| `ai_reviewer_tripped` | reviewer payload | AI-reviewer guard tripped |
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
  "subagent_cost_micro_usd": 0,
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
- `input_tokens` is the **total** prompt the model saw — uncached
  delta + cache reads + cache writes — across every provider. Provider
  bindings normalise to this convention at the snapshot boundary
  (see `AnthropicStreamState::cost` and `BedrockStreamState::cost`),
  so an OpenAI cache-hit turn and an Anthropic cache-hit turn on the
  same prompt shape report comparable totals. The cached share is
  preserved separately on the `cost` payload in `trace.jsonl` as
  `cached_input_tokens` / `cache_write_input_tokens`.
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
| `expect_input_tokens_per_turn` | minor | any turn's input tokens > `expect.max_input_tokens_per_turn` |
| `expect_final_text_contains` | minor | the last turn's assistant_text missing a required substring |
| `expect_no_tool_errors` | minor | any `Error`/`Cancelled` `tool_call_completed` and `expect.no_tool_errors = true` |
| `expect_finish_reason` | major | any completed turn matches `expect.finish_reason_not` — literal match against the provider's `finish_reason`, or the sentinel `"stop_no_action"` (stop + zero tool calls) |
| `expect_dropped_tool_calls` | major | total dropped chat-completions tool-call frames exceeds `expect.max_dropped_tool_calls` |

Additional bundled rules cover graph-overfetch patterns, missing confidence
labels, unsupported or failed TUI actions, platform mismatch, unfired scripted
actions, user-input auto-cancel, denied-tool-call UX, cross-turn duplicate tool
calls, post-compaction failures, empty assistant text, subagent and MCP failures,
cost warnings, AI-reviewer trips, length truncation, sandbox degradation, slow
first token, compaction loops, deferred findings, and dropped-tool-call
expectations. `crates/squeezy-eval/src/findings.rs` is the source of truth for
the full rule inventory.

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
    "cost_display": "$0.0229",
    "total_cost_with_subagents_micro_usd": 22900,
    "parent_cost_micro_usd": 22900,
    "subagent_cost_micro_usd": 0
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
squeezy-eval diff <run-a> <run-b> [--format markdown|json] [--schema-check]
squeezy-eval check <dir> [--fail-on findings,expectations,errors]
                          [--junit <path>]
                          [--out <dir>]
                          [--parallelism <n>]
                          [--input-baseline <json>]
                          [--input-tolerance <fraction>]
                          [--update-baseline]
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
  out (provider not configured, IO error, etc.), `input-regression` =
  total input tokens exceeded a saved baseline beyond tolerance. Default is
  `expectations,errors`.
- `--junit <path>`: write a JUnit XML summary; one `<testcase>` per scenario.
- `--quiet` (on `run`): suppress the default live narration. Without it, `squeezy-eval run` streams squeezy's activity to stdout as it happens — step boundaries, tool calls (`🔧 name(args)` + `↳ ✅/❌ status (bytes)`), the assistant's streaming text rendered inline, approvals, slash commands, and findings the moment each rule fires. Use `--quiet` for CI or when piping output into a pager. `check` always runs quietly because per-scenario PASS/FAIL is the right granularity for batch mode.
- `--schema-check` (on `diff`): refuse to compare runs with different trace
  schema versions.
- `--parallelism <n>` (on `check`): run multiple scenarios concurrently.
- `--input-baseline <json>`, `--input-tolerance <fraction>`, and
  `--update-baseline` (on `check`): compare total input tokens against a saved
  per-scenario baseline, tolerate a bounded increase, and record only newly-seen
  baselines when explicitly requested.

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

- `gpt-5.4-mini` is priced in `crates/squeezy-llm/src/models.json` at
  `$0.75/1M` input tokens, `$0.075/1M` cached-input tokens, and `$4.50/1M`
  output tokens. A trivial nav prompt against the squeezy repo lands around
  14k-50k input tokens because the system prompt + repo profile is substantial.
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
- **Slash-command coverage** is full when `drive_tui = true` and partial
  when it is false. Non-drive runs still record visual-only commands as
  `tui_only:<command>` action statuses, but they do not paint overlays or
  mutate TUI-only state.
- **`wait_for: tool_call` is signal-only.** Scenarios that want
  concurrent action dispatch attach `when.on_tool` to the action they
  want fired mid-stream.
- **TUI capture is cell-grid based, not image based.** `frames_tui.jsonl`
  records rendered cells, ANSI, and plain text from the headless backend;
  it is suitable for assertions and review but does not produce screenshots.
- **No record/replay of an eval trace through the agent.** That's a
  different problem from `squeezy-cli sessions replay`.
- **Single-machine runner.** `squeezy-eval check` can run scenarios in
  parallel with `--parallelism`, but each worker still runs in this process
  and shares process-level env mutation surfaces such as TUI palette pinning.
