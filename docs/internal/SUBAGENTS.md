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

The catalog currently defines three roles:

| Role     | Mutates files | Default model | Purpose                                                              |
| -------- | ------------- | ------------- | -------------------------------------------------------------------- |
| Explorer | No            | Cheap         | Graph-first codebase exploration; the `explore` control tool.        |
| Planner  | No            | Parent        | Read-only implementation planning; the `delegate_plan` control tool. |
| Reviewer | No            | Cheap         | Read-only diff review; the `delegate_review` control tool.           |

`delegate` keeps its existing broad-research behavior and is intentionally
not overlaid by any role, so it retains access to `plan_patch` and skill
discovery for broad research tasks.

## Control Tools

The model can spawn subagents through these advertised tools:

- `delegate(prompt, scope?)` — broad research subagent.
- `explore(prompt, scope?, thoroughness?)` — Explorer role.
- `delegate_plan(goal, scope?)` — Planner role.
- `delegate_review(scope?, prompt?)` — Reviewer role.

All four are gated on `subagents.enabled`. `explore` is additionally gated
on `subagents.explore_enabled`.

## Permission Derivation

Subagents do not inherit the parent's full tool-permission set. Each spawn
in `subagent_allowed_tools` (in `crates/squeezy-agent/src/lib.rs`) starts
from the parent's advertised tools, intersects them with the per-kind
allow-list (the role catalog for `explore`, `delegate_plan`,
`delegate_review`; a curated research list for `delegate`; empty for
`doc_help`), then drops anything whose `PermissionCapability` is not
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
`delegate`, `explore`, `delegate_plan`, or `delegate_review`, and the
subagent loop denies any tool the model calls that is not in its
allow-list. Tests in `roles_tests.rs` and `lib_tests.rs` lock this in.

If you add a new role to the catalog, the
`no_role_advertises_subagent_control_tools` test will catch any leak of
control tools into the role's allow-list.

## Concurrency Cap

`SubagentRegistry` enforces a per-parent breadth cap of
`SUBAGENT_MAX_CONCURRENT` (currently 4). Each `handle_subagent_call` site
takes a `SubagentLease` before running and drops it on completion. When
the cap is reached, the control tool returns `ToolStatus::Denied` with
status `"capped"` rather than queueing or blocking. This keeps fanout
flat and predictable rather than letting one model turn spawn an
unbounded swarm.

## Cancellation

Each subagent receives a `CancellationToken` derived from the parent
turn's token via `context.cancel.child_token()`. When the parent turn is
cancelled, the child token fires immediately. Model streaming and tool
execution race against the token and return `ToolStatus::Cancelled`
within the documented drain target. The lease drops on the way out so
the next subagent can start.

## What This Document Is Not

This document does not describe Codex-style recursive or persistent
agent trees, cloud-style agent fleets, or interactive agents. Squeezy's
subagents are deliberately flat, one-shot, and in-process. If a future
feature needs a different shape, propose it explicitly and update this
document — do not extend the role catalog into something it is not.
