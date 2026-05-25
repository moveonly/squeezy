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
- A status: `Active` or `Roadmap`.

The catalog currently defines four roles:

| Role     | Status   | Mutates files | Default model | Purpose                                                            |
| -------- | -------- | ------------- | ------------- | ------------------------------------------------------------------ |
| Explorer | Active   | No            | Cheap         | Graph-first codebase exploration; the `explore` control tool.      |
| Worker   | Roadmap  | (Yes)         | Parent        | Future mutation-capable worker; no model-visible tool yet.         |
| Planner  | Active   | No            | Parent        | Read-only implementation planning; the `delegate_plan` control tool. |
| Reviewer | Active   | No            | Cheap         | Read-only diff review; the `delegate_review` control tool.         |

`delegate` keeps its existing broad-research behavior and is intentionally
*not* overlaid by the Worker role — the Worker role is roadmap, and pinning
delegate to it would strip access to `plan_patch` and skill discovery.

## Control Tools

The model can spawn subagents through these advertised tools:

- `delegate(prompt, scope?)` — broad research subagent.
- `explore(prompt, scope?, thoroughness?)` — Explorer role.
- `delegate_plan(goal, scope?)` — Planner role.
- `delegate_review(scope?, prompt?)` — Reviewer role.

All four are gated on `subagents.enabled`. `explore` is additionally gated
on `subagents.explore_enabled`.

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
