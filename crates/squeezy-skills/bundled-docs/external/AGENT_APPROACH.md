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

## Graph-First Navigation

Squeezy builds a local semantic graph from tree-sitter parsers, workspace facts,
and language-specific heuristics. Navigation tools return compact evidence
packets with paths, spans, hashes, confidence, freshness, provenance, and next
actions. Unsupported languages and excluded files are reported as fallback
inputs rather than graph-confident answers.

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
tools create checkpoints so recent workspace edits can be inspected, undone, or
reverted.

Verification is explicit. The agent uses local build, test, formatter, linter,
or benchmark commands when the task calls for evidence; navigation tools do not
silently run compilers or external services.
