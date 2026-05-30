# Hooks Scope

Squeezy does not expose a hook framework. Tool lifecycle, compaction, and
prompt assembly are closed paths owned by the agent crate. This document
records the scope decision per use-case so we can decline incremental hook
extensions consistently and revisit only on concrete user demand.

## Session-start and per-prompt enrichment

The enrichment surface today is static. `squeezy-agent` stitches two sources
into `config.instructions` exactly once at session start
(`crates/squeezy-agent/src/lib.rs:1369-1402`):

- Repo doc: concatenated `AGENTS.md` content, capped by
  `ContextCompactionConfig::repo_doc_max_bytes`.
- User memory: contents of `~/.squeezy/memory.md`, capped by
  `ContextCompactionConfig::user_memory_max_bytes`
  (`crates/squeezy-core/src/lib.rs:2725`).

Both caps are config-driven; setting either to `0` disables the corresponding
ingestion. There is no per-prompt hook, no callback that can append text to a
running turn, and no way to fold dynamic facts (today's date, current git
branch, last build status, recent test failures, on-call rotation) into the
base prompt without the user editing `memory.md` or invoking `/context` by
hand.

A typical alternative shape is a pair of `SessionStart` and
`UserPromptSubmit` hook events that return `additional_context` strings
the agent folds into developer instructions.

### Deferred design

We are not adding a hook framework. If a future user request justifies the
work, the minimal alternative is a startup-time enrichment registry that:

- Reads a fixed list of declared local commands from config (e.g.
  `[[enrichment]] name = "git_branch", command = ["git", "branch", "--show-current"]`).
- Runs each command once at session start under the existing shell sandbox
  with a short per-command timeout and a small combined output cap.
- Folds each stdout block into `config.instructions` next to the existing
  repo-doc and memory blocks, with a header that names the source so the
  model can attribute the fact.
- Surfaces per-command success/failure as a session-log event in the same
  shape as `user_memory_ingested`.

This stays inside `squeezy-agent`, reuses the sandbox and session-log
plumbing, and avoids introducing a hook trait, per-event scopes, or a plugin
runtime. A per-prompt variant is explicitly out of scope for the first cut:
session-start enrichment covers the highest-leverage facts (branch, current
date, last test status) without the latency cost of running shells on every
user turn.

The decision to defer is intentional: session-start enrichment is the
load-bearing knob, and a per-prompt variant can land later once we have
profiling on how much the deferred hook costs.
