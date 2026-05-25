# Tool-Call Saving Strategy

Squeezy treats tool calls as a budgeted resource. The agent should spend calls on
deterministic local evidence, avoid repeating work it already paid for, and keep
model-facing tool output compact enough to be useful.

## Implemented Foundation

- **Bounded fallback tools.** `glob`, `grep`, `read_file`, `read_tool_output`,
  `write_file`, and `shell` return structured results with cost hints, output
  caps, and sha256 receipts.
- **Edit-verify loop tools.** `plan_patch` builds a graph-aware evidence packet
  (impacted symbols, callers, references, tests, configs, owners, locality
  scoring) before mutation, and `apply_patch` applies search-replace blocks with
  a required `expected_sha256`, multiple-match guard, locality warnings,
  optional dry-run preview, and an attached checkpoint for rollback when
  checkpointing is enabled.
- **Ignore-aware search.** `grep` respects ignore files by default and requires
  `include_ignored=true` when ignored paths are intentionally needed. `glob`
  follows the same default.
- **Cheap path discovery.** `glob` answers path-pattern questions without
  reading file contents.
- **Search output modes.** `grep` supports `content`, `files_with_matches`, and
  `count` modes so broad exploration does not always return matching lines.
- **Stable tool surface.** Core tool schemas are sent in deterministic order
  and discoverable schemas are appended in first-load order so provider-side
  prompt/cache prefixes can remain stable.
- **Lazy schema loading.** Long-tail and MCP tools are advertised in a compact
  `tools_index` by default. The always-core `load_tool_schema` tool attaches a
  discoverable tool's full schema when the model needs it, and later rounds in
  the same session reuse the expanded schema set. The `tools_index` text is
  intentionally byte-stable across rounds: a tool stays listed in the index
  even after its schema has been attached, so the provider's prompt-cache
  prefix does not invalidate every time the model loads a new tool. The
  `tools_index` and `load_tool_schema` are an advisory hint, not an enforced
  precondition: the registry still owns the actual executors, and the
  permission engine remains the real safety boundary, so a tool call made
  without a prior `load_tool_schema` is routed through the normal permission
  path rather than refused on schema grounds.
- **Parallel read/search calls.** Independent `glob`, `grep`, `read_file`, and
  `read_tool_output` calls can run concurrently while write and shell calls stay
  serialized. Graph-backed navigation tools use the same read-only execution
  path.
- **Graph-backed navigation.** `repo_map`, `decl_search`,
  `definition_search`, `reference_search`, `upstream_flow`,
  `downstream_flow`, `symbol_context`, `hierarchy`, and `read_slice` answer
  common code questions from persisted graph partitions and return compact
  evidence packets instead of raw file dumps.
- **Exploration compiler.** Common navigation prompts are classified before the
  first model request and routed through deterministic graph-first tool plans.
  The resulting evidence packets are inserted into the model input as ordinary
  tool results, and confident navigation turns refuse premature `read_file`
  calls until the planner's preflight block has executed (the planner is
  advisory; the guard is lifted after preflight even if individual graph
  tools returned non-`Success` statuses). The test-pairing plan's filesystem
  glob (`**/*test*.rs`) is currently Rust-specific, in line with Squeezy's
  Rust-first navigation scope; on non-Rust workspaces it produces an empty
  result and is otherwise inert.
- **Output spill previews.** Large tool outputs are written to a local
  content-addressed store. The model receives a compact preview plus a handle
  for fetching exact ranges. Spill previews also carry the original output
  receipt so repeated large results can be deduped even when each call receives
  a different spill handle.
- **Verify-loop output shaping.** `shell` and `verify` default to compact
  shaped output, with an `output_mode="raw"` override for exact stdout/stderr.
  Shell capture reserves budget for both stdout and stderr, then rebalances
  unused capacity, so a noisy progress stream does not starve diagnostics from
  the other stream.
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
  Successful web results are marked as remote evidence and include source URL
  fields, retrieval time, citation metadata, redacted quote hashes, and
  cache-receipt metadata. Quote byte caps are enforced after redaction so
  secret-looking remote text is scrubbed before it is truncated, spilled,
  logged, or sent back to the model.
- **Provider tool calls.** The OpenAI Responses and Anthropic Messages providers
  expose documented function tools and feed tool outputs back into the model
  loop.
- **Per-turn broker metrics.** Tool-call, read-byte, search-file, receipt-hit,
  spill, denial, provider-token, cache, and estimated-cost counters are tracked
  per turn and surfaced to the TUI/harness.
- **Subagent isolation.** `delegate` and `explore` run bounded child model/tool
  loops with their own context and budgets. The parent receives a structured
  summary plus receipt hashes, not the child's intermediate tool outputs.
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
- `SQUEEZY_EXPLORATION_COMPILER=off` disables the deterministic graph-first
  planner for comparison runs or debugging. It is enabled by default.
- `SQUEEZY_WEB_PERMISSION` controls the allow/ask/deny policy for `websearch`
  and `webfetch`. The default is `ask`.
- `webfetch.output_byte_cap` and `websearch.output_byte_cap` are per-call tool
  arguments that bound the redacted quote text returned to the model. They do
  not change the HTTP response byte caps used to protect the local process.
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
- **Diff awareness.** The current branch diff and recently changed files are
  queryable as compact summaries through `diff_context`. `read_slice` also
  supports `read_mode="diff"` for source bytes changed against `worktree`
  (staged, unstaged, and untracked changes vs `HEAD`), `branch_base` (the
  default-branch merge base), `index` (staged changes), or `last_receipt`.
  Ranges today are line/byte hunks derived from git, not symbol spans; a
  graph-driven structural variant that returns enclosing symbol spans is a
  follow-up.
- **Last-receipt fallback semantics.** `last_receipt` compares the requested
  window against the most recent model-visible read snapshot for that exact
  `(path, start_byte, end_byte)` tuple, returns a receipt stub when the file
  hash is unchanged, and otherwise falls back to `worktree` with a
  `baseline_fallback` label (`last_receipt_store_unavailable`,
  `last_receipt_snapshot_missing`, `last_receipt_window_mismatch`,
  `last_receipt_current_file_unavailable`, or `last_receipt_store_error`) so
  the model can tell apart "no snapshot" from "snapshot for a different
  window" from "transient IO error".
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
