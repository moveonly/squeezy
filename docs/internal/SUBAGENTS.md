# Subagents And Delegation

Squeezy spawns short-lived, isolated subagents to keep specialized work off
the parent turn's context and budget. This document records the current
shape so future changes do not accidentally re-introduce recursive or
persistent agent topologies.

## Roles

Subagent behavior is parameterized by a role drawn from a static catalog in
`crates/squeezy-agent/src/roles.rs`. Each role pins:

- An instruction overlay used as the subagent's system prompt.
- A read-only or planning-only allow-list of tool names.
- A model policy: reuse the parent model, or downshift to a cheap model.
- A reasoning-effort hint for providers that accept one.

The catalog currently defines three role overlays:

| Role     | Mutates files | Default model | Purpose                                                              |
| -------- | ------------- | ------------- | -------------------------------------------------------------------- |
| Explorer | No            | Cheap         | Graph-first codebase exploration; the `explore` control tool.        |
| Planner  | No            | Parent        | Read-only implementation planning; the `delegate_plan` control tool. |
| Reviewer | No            | Cheap         | Read-only diff review; the `delegate_review` control tool.           |

`delegate`, `doc_help`, and fork-mode `skill` subagents are real
`SubagentKind` variants but are intentionally not overlaid by a role.
`delegate` keeps its existing broad-research behavior so it retains access to
`plan_patch` and skill discovery for broad research tasks. `doc_help` answers
from the bundled help corpus with no file tools. `skill` is wired for bounded
fork-mode skill execution with the skill body as the subagent instruction
overlay, but current fork-mode skills are advertised to the parent and rely on
normal delegation rather than automatic `SubagentKind::Skill` dispatch.

## Control Tools

The model can spawn subagents through these advertised tools:

- `delegate(prompt, scope?)` — broad research subagent.
- `explore(prompt, scope?, thoroughness?)` — Explorer role.
- `delegate_plan(goal, scope?)` — Planner role.
- `delegate_review(scope?, prompt?)` — Reviewer role.
- `delegate_chain(steps[])` — up to 16 sequential delegate steps; each step's
  summary is substituted into the next step's prompt via `{previous}`.

All five are gated on `subagents.enabled`. `explore` is additionally gated on
`subagents.explore_enabled`. The internal `doc_help` subagent (the `/help`
doc-lookup fallback) is also gated on `subagents.enabled` but is never
advertised as a model-callable tool.

`delegate` and `delegate_plan`/`delegate_review` have different concurrency
characteristics: `delegate*` calls in the same model turn are fanned out
concurrently under `buffer_unordered(max_concurrent)`; `explore` runs
single-shot (serial within a turn) to avoid races on its exploration-state
lock. `delegate_chain` manages its own sequential step loop and does not
join the concurrent batch.

`delegate` has an additional anti-redundancy gate: when the parent turn has
already gathered substantial context, a whole-task `delegate` can be denied as
pure overhead while the parent keeps the direct read/search/graph tools it
already has. The scoped `delegate_plan` and `delegate_review` tools do not use
that gate.

## Permission Derivation

Subagents do not inherit the parent's full tool-permission set. Each spawn
in `subagent_allowed_tools` (in `crates/squeezy-agent/src/lib.rs`) starts
from the parent's advertised tools, intersects them with the per-kind
allow-list (the role catalog for `explore`, `delegate_plan`,
`delegate_review`; a curated research list for `delegate`; empty for
`doc_help`; the delegate read-only research set for `skill`), then drops
anything whose `PermissionCapability` is not
`Read` or `Search`. The capability filter is the load-bearing safety
guarantee: even if a future role allow-list accidentally names a mutating
tool, the filter still strips `Edit`, `Shell`, `Network`, `Mcp`, `Git`,
`Compiler`, and `Destructive` from the advertisement the subagent sees.
`explore_subagent_cannot_call_write_file` and
`typed_subagents_filter_to_read_search_capability` in `lib_tests.rs`
lock this in.

## Flat Spawning Invariant

Subagents may never spawn other subagents. The invariant is enforced *by
construction*: the per-role `allowed_tools` lists do not contain
`delegate`, `explore`, `delegate_plan`, `delegate_review`, or
`delegate_chain`, and the subagent loop denies any tool the model calls that
is not in its allow-list. Tests in `roles_tests.rs` and `lib_tests.rs` lock
this in.

If you add a new role to the catalog, the
`no_role_advertises_subagent_control_tools` test will catch any leak of
control tools into the role's allow-list.

## Concurrency Cap

`SubagentRegistry` enforces a per-parent breadth cap of
`SUBAGENT_MAX_CONCURRENT` (default 20, overridable via `subagents.max_concurrent` in TOML or `SQUEEZY_SUBAGENT_MAX_CONCURRENT`). Each `handle_subagent_call` site
takes a `SubagentLease` before running and drops it on completion. When
the cap is reached, the control tool returns `ToolStatus::Denied` with
status `"capped"` rather than queueing or blocking. This keeps fanout
flat and predictable rather than letting one model turn spawn an
unbounded swarm.

Each admitted subagent also gets its own bounded loop:

- `max_tool_calls_per_call`, `max_tool_bytes_read_per_call`, and
  `max_search_files_per_call` replace the inherited parent per-turn caps.
- `max_model_rounds` bounds the model/tool loop.
- `max_runtime_secs` applies an optional wall-clock timeout; `0` disables this
  wall-clock cap while leaving cancellation and round caps in place.
- `max_summary_tokens` is the fallback output ceiling for normal subagents when
  the parent has not set `max_output_tokens`. `doc_help` keeps its own answer
  floor.
- `include_transcript` is false by default; when true, the structured tool
  result includes the child's assistant/tool trace for debugging.

## Cancellation

Each subagent receives a `CancellationToken` derived from the parent
turn's token via `context.cancel.child_token()`. When the parent turn is
cancelled, the child token fires immediately. Model streaming and tool
execution race against the token and return `ToolStatus::Cancelled`
within the documented drain target. The lease drops on the way out so
the next subagent can start.

## Parent Visibility

Subagent tool events are drained on a hidden channel so high fanout cannot fill
the parent event buffer. The parent receives explicit lifecycle events:
`SubagentStarted`, `SubagentToolResult`, `SubagentCompleted`,
`SubagentFailed`, and `SubagentRejected`. The TUI uses
those events for the first-class subagent pane below the status line and for
the selected subagent transcript view. `ToolProgress` heartbeats are forwarded
so a long-running subagent keeps the parent turn and eval harness visibly
alive.

Subagent cost and usage are folded into the parent turn. `TurnMetrics`
separately buckets the operator-facing kinds (`delegate`, `explore`, `plan`,
`review`); helper kinds such as `doc_help` are intentionally not bucketed.

## What This Document Is Not

This document does not describe recursive or persistent agent trees,
cloud-style agent fleets, or interactive agents. Squeezy's subagents
are deliberately flat, one-shot, and in-process. If a future
feature needs a different shape, propose it explicitly and update this
document — do not extend the role catalog into something it is not.
