# Tools And Commands

Squeezy tools are local capabilities exposed to the agent under the current
session mode and permission policy. Some tools are always-core, some are
configured as core tools, and some are discoverable through lazy schema loading.

## Navigation And Search

- `repo_map`: compact architecture map, language counts, coverage, unsupported
  files, and next graph actions.
- `decl_search`, `definition_search`: graph-backed declaration lookup and
  disambiguation.
- `reference_search`: symbol-bound or broad heuristic references.
- `upstream_flow`, `downstream_flow`: callers, callees, references, and bounded
  call-chain context.
- `hierarchy`, `symbol_context`: containment and focused symbol context.
- `read_slice`: exact bounded source slices by symbol, byte range, line range,
  or changed diff ranges.
- `grep`, `glob`, `read_file`: bounded fallback search and reads.
- `diff_context`: current Git change set with compact semantic cross-references.

## Editing, Shell, And Verification

- `plan_patch`: plan a search-replace edit using graph impact context.
- `apply_patch`, `write_file`: mutate workspace files with stale-content checks
  and checkpoint coverage.
- `shell`: run a bounded local shell command after permission checks and sandbox
  planning.
- `verify`: run bounded local verification, defaulting to the current diff
  scope.
- `refresh_compiler_facts`: explicitly refresh cached Cargo metadata and
  optional diagnostics for Rust workspaces.

## State, Skills, Web, And MCP

- `read_tool_output`: retrieve a spilled large tool result by handle.
- `list_skills`, `load_skill`: discover and activate local `SKILL.md`
  instructions.
- `checkpoint_list`, `checkpoint_show`, `checkpoint_undo`,
  `checkpoint_revert`: inspect and roll back recent mutating tool checkpoints
  when checkpointing is enabled.
- `websearch`, `webfetch`: permission-gated external lookup through configured
  web tooling.
- External MCP tools are namespaced by server and follow each server's
  configured permission policy.

## TUI Slash Commands

The TUI supports local commands for common work without requiring a model turn:

- `/help [topic]`: local Squeezy help.
- `/plan`, `/build`: switch session mode.
- `/cost`, `/context`: show accounting and context snapshots.
- `/attach`, `/attachments`, `/detach`: manage context attachments.
- `/compact`, `/pin`, `/pins`, `/unpin`: manage context compaction and pins.
- `/sessions`, `/session <id>`, `/resume <id>`, `/session-export`,
  `/session-cleanup`: inspect, resume, export, or clean up sessions.
- `/feedback`, `/report`: prepare consented maintainer feedback or bug reports.
- `/copy`, `/collapse`, `/expand`, `/verbosity`, `/tool-verbosity`, `/jobs`:
  local TUI display and job controls.

The exact available tool set can change with mode, configuration, permissions,
and enabled MCP servers. Use `squeezy config inspect` to inspect the effective
configuration.
