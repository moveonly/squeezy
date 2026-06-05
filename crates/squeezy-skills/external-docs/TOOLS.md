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
- `apply_patch`, `write_file`, `notebook_edit`: mutate workspace files with
  stale-content checks and checkpoint coverage. Mutating tools preflight target
  paths before any filesystem write and refuse paths outside writable roots or
  under protected metadata directories such as `.git`, `.squeezy`, and
  `.agents`.
- `shell`: run a bounded local shell command after permission checks and sandbox
  planning; supports `tty=true` for PTY-backed commands and exposes
  `squeezy ask` to approved shell children when in-flight permission prompts
  are available.
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
- `notes_remember`, `notes_recall`, `observations`: record or retrieve compact
  local notes and observations.
- `websearch`, `webfetch`: permission-gated external lookup through configured
  web tooling.
- `mcp_list_resources`, `mcp_list_resource_templates`, `mcp_read_resource`:
  inspect resources exposed by enabled MCP servers.
- External MCP tools are namespaced by server and follow each server's
  configured permission policy.

## TUI Slash Commands

The TUI supports local commands for common work without requiring a model turn:

- `/help [topic]`: local Squeezy help.
- `/config [section]`, `/model`, `/permissions`, `/mcp`: open local
  configuration views.
- `/plan`, `/build`: switch session mode.
- `/plans`: manage persisted plan-mode artifacts.
- `/cost`, `/context`: show accounting and context snapshots.
- `/attach`, `/attachments`, `/detach`: manage context attachments.
- `/compact`, `/pin`, `/pins`, `/unpin`: manage context compaction and pins.
- `/diff`: show tracked and untracked workspace changes.
- `/sessions`, `/session <id>`, `/session rename <name>`,
  `/session label <name>`, `/resume <id>`, `/session-export`,
  `/session-export-html`, `/clear`: inspect, annotate, resume, export
  sessions, or clear the conversation.
- `/fork`: branch the current session into a sibling session.
- `/feedback`, `/report`: prepare consented maintainer feedback or bug reports.
- `/tasks`, `/task <id>`, `/task-cancel <id>`: inspect and cancel background
  tasks.
- `/effort`, `/cheap`, `/parent`, `/verbosity`, `/tool-verbosity`: local model
  and display controls.
- `/checkpoints`, `/checkpoint <id>`, `/undo`, `/revert-turn <id>`:
  available when checkpointing is enabled.
- `/router [on|off]`: toggle cheap-model turn routing for the session; without
  args opens the routing config view.
- `/theme [name]`: switch the TUI color theme. Built-in themes: `default`,
  `bright`, `fun`, `catppuccin`, `high-contrast`. Use `/theme default` to
  reset.
- `/spinner [name]`: set the working-status spinner.
- `/reviewer`: show recent AI reviewer auto-decisions.
- `/statusline`: configure the custom status line footer.
- `/keymap`: list current key bindings.

Prompt templates in `~/.squeezy/prompts/` and `<workspace>/.squeezy/prompts/`
activate by their slash name, such as `/review` for `review.md`. See
[PROMPT_TEMPLATES.md](PROMPT_TEMPLATES.md).

The exact available tool set can change with mode, configuration, permissions,
and enabled MCP servers. Use `squeezy config inspect` to inspect the effective
configuration.
