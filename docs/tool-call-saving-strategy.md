# Tool-Call Saving Strategy

Squeezy treats tool calls as a budgeted resource. The agent should spend calls on
deterministic local evidence, avoid repeating work it already paid for, and keep
model-facing tool output compact enough to be useful.

## Implemented Foundation

- **Bounded fallback tools.** `glob`, `grep`, `read_file`, `read_tool_output`,
  `write_file`, and `shell` return structured results with cost hints, output
  caps, and sha256 receipts.
- **Ignore-aware search.** `grep` respects ignore files by default and requires
  `include_ignored=true` when ignored paths are intentionally needed. `glob`
  follows the same default.
- **Cheap path discovery.** `glob` answers path-pattern questions without
  reading file contents.
- **Search output modes.** `grep` supports `content`, `files_with_matches`, and
  `count` modes so broad exploration does not always return matching lines.
- **Stable tool surface.** Tool schemas are sorted deterministically so
  provider-side prompt/cache prefixes can remain stable.
- **Parallel read/search calls.** Independent `glob`, `grep`, `read_file`, and
  `read_tool_output` calls can run concurrently while write and shell calls stay
  serialized. Graph-backed navigation tools use the same read-only execution
  path.
- **Graph-backed navigation.** `repo_map`, `decl_search`,
  `definition_search`, `reference_search`, `upstream_flow`,
  `downstream_flow`, `symbol_context`, `hierarchy`, and `read_slice` answer
  common code questions from persisted graph partitions and return compact
  evidence packets instead of raw file dumps.
- **Output spill previews.** Large tool outputs are written to a local
  content-addressed store. The model receives a compact preview plus a handle
  for fetching exact ranges. Spill previews also carry the original output
  receipt so repeated large results can be deduped even when each call receives
  a different spill handle.
- **Verify-loop output shaping.** `shell` and `verify` default to compact
  shaped output, with an `output_mode="raw"` override for exact stdout/stderr.
  Squeezy-owned Rust verification commands request Cargo JSON output where
  supported and parse it into warnings, errors, failures, and exit summaries.
  Cargo JSON output is interleaved with libtest's plain-text harness lines, so
  the shaper also keeps those failure markers ("test result:", panics,
  `FAILED`) when JSON is present. Direct shell commands are not rewritten;
  recognized structured output is parsed when present, and otherwise narrow
  line shapers drop progress and repeated noise while preserving error,
  warning, failure, and status lines plus a tail window of context. Spill
  handles continue to store the unshaped raw result for exact follow-up
  reads.
- **Receipt-backed output stubs.** Repeated successful read-style tool outputs
  with the same receipt are replaced with a compact stub that points back to
  the first model-visible result. The compact receipt ledger is persisted under
  the cache root, so identical outputs can be stubbed in later sessions too.
  Outputs omitted by the aggregate result budget are not remembered as seen.
- **Aggregate result budgets.** A round with many parallel tools enforces a
  combined model-facing output cap, not only per-tool caps.
- **Unified permission engine.** Every tool call is converted into a structured
  permission request with capability, target, risk, metadata, and suggested
  persistence rules before execution. Compatibility `allow`/`ask`/`deny`
  defaults still work, and ordered `[[permissions.rules]]` entries can target
  command prefixes, domains, paths, or tool families. Session approvals from
  the TUI are layered on top of file rules and take effect immediately for
  later tool calls in the same process, so an "Allow user/project rule" choice
  does not require restarting the agent to be honored. Every verdict is logged
  via the `squeezy::permissions` tracing target with capability, target, risk,
  action, matched-rule source, and reason fields.
- **Destructive safety net.** Allow rules on the `destructive` capability are
  refused both at config load time and at approval-persistence time, so an
  approved "Allow project rule" for `rm -rf node_modules` cannot quietly turn
  into blanket permission for `rm:*` later.
- **Permission-gated mutation.** Edit, shell, compiler/verify, git, network,
  and destructive tool capabilities route through the same policy engine before
  execution.
- **Permission-gated web access.** `websearch` performs current/external
  discovery through a configurable Exa MCP endpoint, and `webfetch` retrieves
  specific HTTP(S) URLs with bounded response sizes, text/HTML shaping, content
  receipts, host-visible approval summaries, and cross-host redirect stops.
- **Provider tool calls.** The OpenAI Responses and Anthropic Messages providers
  expose documented function tools and feed tool outputs back into the model
  loop.
- **Per-turn broker metrics.** Tool-call, read-byte, search-file, receipt-hit,
  spill, denial, provider-token, cache, and estimated-cost counters are tracked
  per turn and surfaced to the TUI/harness.
- **Anonymous telemetry hooks.** Tool completions and turn aggregates emit
  typed telemetry with sequence numbers, timings, statuses, and numeric cost
  counters. Tool arguments, prompts, paths, commands, URLs, and content are not
  sent.

## Runtime Knobs

- `SQUEEZY_MAX_PARALLEL_TOOLS` controls how many read-only tool calls may run in
  one parallel batch.
- `SQUEEZY_TOOL_SPILL_THRESHOLD_BYTES` controls when a tool result is stored on
  disk and replaced with a preview.
- `SQUEEZY_TOOL_PREVIEW_BYTES` controls the preview size sent to the model for
  spilled outputs.
- `SQUEEZY_MAX_TOOL_RESULT_BYTES_PER_ROUND` controls the combined model-facing
  output budget for one tool round.
- `SQUEEZY_TOOL_OUTPUT_RETENTION_DAYS` controls cleanup of stored tool-output
  handles.
- `SQUEEZY_MAX_TOOL_CALLS_PER_TURN` controls the per-turn tool-call cap.
- `SQUEEZY_MAX_TOOL_BYTES_READ_PER_TURN` controls the per-turn read-byte cap.
- `SQUEEZY_MAX_SEARCH_FILES_PER_TURN` controls the per-turn search file-scan
  cap.
- `SQUEEZY_WEB_PERMISSION` controls the allow/ask/deny policy for `websearch`
  and `webfetch`. The default is `ask`.
- `SQUEEZY_SHELL_PERMISSION_CLASSIFIER` controls the narrow LLM fallback for
  ambiguous shell commands. Disabled by default to keep the per-turn token
  budget predictable: every ambiguous shell call would otherwise cost one
  extra round-trip with the agent model. Deterministic command analysis runs
  first; the classifier can require approval or deny, but it does not silently
  allow a command, and unparseable classifier output now keeps the verdict at
  `ask` instead of being heuristically guessed.
- `SQUEEZY_EXA_MCP_URL` controls the MCP endpoint used by `websearch`. The
  default is `https://mcp.exa.ai/mcp`.
- `SQUEEZY_EXA_API_KEY_ENV` names the environment variable read for the Exa API
  key. The default is `EXA_API_KEY`.
- `SQUEEZY_TELEMETRY=off` disables anonymous product telemetry.

## Later Structural Savings

- **MCP tool spill routing.** External MCP tool execution should pass through
  the same spill, preview, and receipt-stub layer once MCP tool execution
  exists.
- **Diff awareness.** The current branch diff and recently changed files should
  be queryable as compact summaries.
- **Deferred tool loading.** Long-tail tools, including MCP tools, should load
  schemas only when the model actually needs them.
- **Provider cache controls.** Provider-specific cache keys and cache-friendly
  request shaping should preserve stable prompt prefixes.
- **Approval persistence.** TUI prompts can allow once, allow a user/project
  rule, deny once, or persist a denial rule. Web approvals can persist
  per-domain rules, and shell approvals can persist command-prefix rules.

## Non-Goals

- Provider-native LLM web search that bypasses Squeezy's tool policy.
- Provider-hosted file search.
- Provider-side code interpreter tools.
- Provider-native shell tools that bypass Squeezy's local permission policy.
- A local general-purpose public-web crawler or search index.
