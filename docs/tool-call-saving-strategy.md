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
  serialized.
- **Output spill previews.** Large tool outputs are written to a local
  content-addressed store. The model receives a compact preview plus a handle
  for fetching exact ranges. Spill previews also carry the original output
  receipt so repeated large results can be deduped even when each call receives
  a different spill handle.
- **Receipt-backed output stubs.** During one turn, repeated successful
  read-style tool outputs with the same receipt are replaced with a compact
  stub that points back to the first model-visible result. Outputs omitted by
  the aggregate result budget are not remembered as seen.
- **Aggregate result budgets.** A round with many parallel tools enforces a
  combined model-facing output cap, not only per-tool caps.
- **Permission-gated mutation.** Edit and shell tools route through
  allow/ask/deny policy before execution.
- **Permission-gated web access.** `websearch` performs current/external
  discovery through a configurable Exa MCP endpoint, and `webfetch` retrieves
  specific HTTP(S) URLs with bounded response sizes, text/HTML shaping, content
  receipts, host-visible approval summaries, and cross-host redirect stops.
- **Provider tool calls.** The OpenAI Responses and Anthropic Messages providers
  expose documented function tools and feed tool outputs back into the model
  loop.

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
- `SQUEEZY_WEB_PERMISSION` controls the allow/ask/deny policy for `websearch`
  and `webfetch`. The default is `ask`.
- `SQUEEZY_EXA_MCP_URL` controls the MCP endpoint used by `websearch`. The
  default is `https://mcp.exa.ai/mcp`.
- `SQUEEZY_EXA_API_KEY_ENV` names the environment variable read for the Exa API
  key. The default is `EXA_API_KEY`.

## Later Structural Savings

- **MCP tool spill routing.** External MCP tool execution should pass through
  the same spill, preview, and receipt-stub layer once MCP tool execution
  exists.
- **Graph-backed navigation.** Symbol lookup, references, call candidates,
  test-of relationships, and span reads should answer common code questions
  without shell/search/read loops.
- **Diff awareness.** The current branch diff and recently changed files should
  be queryable as compact summaries.
- **Deferred tool loading.** Long-tail tools, including MCP tools, should load
  schemas only when the model actually needs them.
- **Provider cache controls.** Provider-specific cache keys and cache-friendly
  request shaping should preserve stable prompt prefixes.
- **Domain-aware web approvals.** Web permissions should eventually support
  durable per-domain policy decisions instead of only a global web scope.

## Non-Goals

- Provider-native LLM web search that bypasses Squeezy's tool policy.
- Provider-hosted file search.
- Provider-side code interpreter tools.
- Provider-native shell tools that bypass Squeezy's local permission policy.
- A local general-purpose public-web crawler or search index.
