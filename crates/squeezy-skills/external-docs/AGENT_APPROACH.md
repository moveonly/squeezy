# Agent Approach

Squeezy is a local-first coding agent. It tries deterministic repository
analysis before asking a model to inspect raw files, and it keeps every tool
response compact enough to preserve model context.

## Local Help First

Questions about Squeezy itself are handled before provider work starts when they
look like product-help requests. `/help <topic>` and matching natural-language
questions are answered from this external docs corpus plus redacted
`squeezy config inspect` output. If the topic is not covered locally, Squeezy
refuses to guess and points to the public docs and repository.

Implementation and debugging requests that merely mention Squeezy stay on the
normal agent path so code work is not replaced with canned help.

## Plan And Build Modes

Squeezy sessions run in either `plan` or `build` mode. The mode can be set with
`--mode plan|build`, `[session].mode`, or TUI slash commands.

- `plan` mode is for exploration and design. Mutating tools are not advertised
  to the model.
- `build` mode is for implementation. Mutating tools are available behind the
  configured permission policy.

The active mode is part of tool advertisement and runtime permission checks, so
a tool hidden from a mode is not only hidden from the prompt; attempts to call it
are also rejected.

## Turn Routing

Squeezy can route straightforward turns to a cheaper model tier while keeping
harder turns on the main model. The router uses a static heuristic for obvious
mechanical commands and optionally a cheap judge model for ambiguous turns.
Turn routing is on by default and can be toggled with `/router on|off` or
configured under `[routing]` and per-provider settings. It never crosses
providers.

## Subagents

For research and doc-help turns, Squeezy spawns isolated subagents. Explore
subagents use read/search/navigation tools only and run on the cheap model tier.
Delegate subagents answer research questions using the main model with the same
navigation tool set. Doc-help subagents answer `/help` escalations from the
inlined bundled doc corpus with no filesystem tools. Subagent tool calls and
model rounds are capped separately from the parent; only the final structured
summary is returned.

## Graph-First Navigation

Squeezy builds a local semantic graph from tree-sitter parsers, workspace facts,
and language-specific heuristics. Navigation tools return compact evidence
packets with paths, spans, hashes, and confidence. Unsupported languages and
excluded files are reported as fallback inputs rather than graph-confident
answers.

The agent should prefer `repo_map`, declaration/reference/flow tools, and
`read_slice` before broad raw file reads. Bounded `grep`, `glob`, and
`read_file` remain available for unsupported languages and ordinary text search.

## Tool Schema And Context Budgeting

Squeezy keeps common control and core tools visible, then exposes other tool
schemas lazily through `load_tool_schema` when `lazy_schema_loading` is enabled.
This keeps initial provider requests smaller without hiding discoverable tools.

Large tool outputs are capped, compacted, or spilled behind receipts. Repeated
reads can return receipt stubs instead of resending bytes. The TUI status line
shows token, cost, context, compaction, and budget-denial counters so the user
can see when context pressure is shaping behavior.

## Permissions And Verification

Tool execution is policy-gated by capability: read, edit, shell, web, compiler,
or MCP. Shell commands have a separate sandbox layer when enabled. Mutating
tools can create checkpoints so recent workspace edits can be inspected, undone,
or reverted when checkpointing is enabled.

Verification is explicit. The agent uses local build, test, formatter, linter,
or benchmark commands when the task calls for evidence; navigation tools do not
silently run compilers or external services.
