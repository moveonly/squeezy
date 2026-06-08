# Hooks Scope

Squeezy has a hook engine, but the public user surface is deliberately narrow.
`squeezy-hooks` defines typed lifecycle events and the agent crate dispatches
them from prompt, tool, permission, compaction, setup, and subagent paths.
Skills may declare shell hooks in `SKILL.md`, but those hooks are inert unless
`[skills].hooks_enabled = true`.

This document records the boundary between the implemented local hook surface
and extension-runtime ideas that remain out of scope.

## Implemented surface

The internal hook system has two layers:

- `AgentHook` / `AgentHookBus`: typed integration hooks for Squeezy-owned
  extensions that can mutate request, tool-call, and tool-result views.
- `HookRegistry` / `HookHandler`: event-indexed dispatch used by skill hook
  scripts and current agent dispatch sites. Handlers registered via
  `register_for_event` are dispatched in O(matching handlers); handlers
  registered via `register` observe all events.

The skill hook frontmatter parser accepts all internal hook events (PascalCase
or snake_case aliases): `PreTurn`, `PreToolUse`, `PostToolUse`,
`PostToolUseFailure`, `PostTool`, `PreCompact`, `PostCompact`,
`SubagentStart`, `SubagentStop`, `PermissionRequest`, `PermissionDenied`,
`UserPromptSubmit`, `SessionStart`, `Stop`, and `Setup`.

Enforcement semantics apply only to `PreToolUse` and `PermissionRequest`: a
non-zero hook exit blocks the action. All other events are observation-only
from the skill hook surface — a non-zero exit is logged but does not affect
the outcome. Skill hook stdout is always ignored; mutations are only available
to typed in-process `AgentHook` handlers.

Skill hook commands run through `sh -c` from the skill directory with
`SQUEEZY_HOOK_PAYLOAD`, `SQUEEZY_SKILL_DIR`, and `SQUEEZY_SKILL_NAME` set.
`matcher` filters tool-scoped payloads by exact `tool_name`; an omitted matcher
means every payload for that event. `once: true` suppresses later executions
only after a successful exit, so a failed first run can retry. Stdout is not
parsed for mutations. A successful exit allows the event; a non-zero exit
returns a deny result where the dispatch site enforces one.

## Prompt enrichment

The static enrichment surface still exists. `squeezy-agent` stitches repo docs,
user memory, generated repo profile context, and selected runtime context into
instructions before issuing requests. The durable local sources include:

- Repo doc: concatenated `AGENTS.md` content, capped by
  `ContextCompactionConfig::repo_doc_max_bytes`.
- User memory: contents of `~/.squeezy/MEMORY.md` when present, falling back to
  lowercase `memory.md`, capped by
  `ContextCompactionConfig::user_memory_max_bytes`.
- Repo profile: generated machine-local repo facts from `~/.squeezy/repos.toml`.

`PreTurn` typed handlers may append `extra_instructions`, and
`UserPromptSubmit` typed handlers may rewrite the prompt. Skill shell hooks do
not currently expose those mutations because their stdout is ignored.

## Still out of scope

The hook engine is not a marketplace, plugin runtime, remote extension host, or
general shell-automation framework. New public hook capabilities should stay
local, opt-in, and testable. If a future user request justifies more prompt
enrichment, the minimal alternative is a startup-time enrichment registry that:

- Reads a fixed list of declared local commands from config (e.g.
  `[[enrichment]] name = "git_branch", command = ["git", "branch", "--show-current"]`).
- Runs each command once at session start under the existing shell sandbox
  with a short per-command timeout and a small combined output cap.
- Folds each stdout block into `config.instructions` next to the existing
  repo-doc and memory blocks, with a header that names the source so the
  model can attribute the fact.
- Surfaces per-command success/failure as a session-log event in the same
  shape as `user_memory_ingested`.

This stays inside `squeezy-agent`, reuses the session-log plumbing, and avoids
introducing remote code-loading or a plugin runtime. A broader per-prompt
shell-enrichment variant remains out of scope until there is profiling that
justifies the latency and trust cost.
