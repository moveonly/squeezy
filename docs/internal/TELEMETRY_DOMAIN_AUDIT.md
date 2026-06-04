# Telemetry Domain Audit

This document captures telemetry coverage ideas and follow-up gaps for the
planned move toward one bounded `squeezy_session_summary` event backed by a
local durable telemetry ledger.

## Subagent Execution Note

The original request asked for 10 subagents. The first attempt did not produce
10 successful audits because the multi-agent environment hit an active thread
limit, and one earlier agent failed with a remote compact/stream disconnect.
That should have been reported immediately before implementation continued.

The follow-up audit was retried with five domains:

- skills and prompt templates
- MCP and external integrations
- subagents, tool loop, approvals, permissions, and failures
- session lifecycle, startup, config, graph, and performance
- local persistence, retry, Worker/PostHog, and summary architecture

## Summary Event Principle

Prefer a local telemetry ledger plus one bounded remote summary:

1. Record local telemetry facts at the moment they happen.
2. Store `occurred_at_ms` and monotonic `sequence`.
3. On session exit, sort by `(occurred_at_ms, sequence)`.
4. Reduce into one `squeezy_session_summary` event with aggregate sections.
5. Store the summary as pending before sending.
6. Drain pending summaries on next startup.
7. Detect prior abnormal sessions and synthesize a summary with an abnormal
   status.

Do not send a raw nested event log to PostHog by default. Use bounded counters,
histograms, top buckets, and `truncated = true` plus dropped counts when a
section exceeds caps.

## Direct-Send Exceptions

Keep these outside the one summary event:

- `/feedback`: explicit user consent and preview/redaction flow.
- `/report`: explicit user consent, redacted archive upload, and separate
  metadata forwarding.
- Fatal errors only when they cannot be durably recorded first. Prefer local
  durable recording and next-start recovery.

## Global Privacy Exclusions

Never send:

- prompts, model responses, reasoning text, or transcript snippets
- file contents, snippets, paths, repository names, session titles, labels
- shell commands, command args, stdout/stderr, command output previews
- URLs, domains from user work, web queries, fetched web content
- API keys, tokens, environment variable values, custom headers
- raw settings values, custom model ids, provider response/request ids
- slash arguments, prompt-template names, tool arguments
- raw hashes of arguments/content/output that can fingerprint user data

Use enums, booleans, counts, duration buckets, byte buckets, and bounded tokens
derived from shipped static identifiers.

## Current PR Coverage

PR #302 currently adds:

- `squeezy_startup_ready`
- `squeezy_session_ended`
- `squeezy_graph_build_completed`
- `squeezy_slash_command_used`
- `squeezy_config_change_committed`

Important limitations:

- Skills are not tracked yet.
- MCP has no dedicated summary section yet.
- Subagents are only represented by session-level `subagent_calls` and
  `subagent_failures`.
- Tool, slash, config, graph, and failure facts should move from remote
  per-event emission into local facts reduced into `squeezy_session_summary`.

## Domain Ideas From Earlier Audits

### Startup And Session Lifecycle

Capture:

- startup route: fresh, direct resume, resume picker fresh, resume picker
  resume, first-run setup fresh
- time to placeholder draw, agent build, snapshots done, first interactive draw
- clean exit status and abnormal-exit detection
- session duration, turn count, prompt queue count buckets, max active jobs
- local pending-summary drain attempts and outcomes

Origin points:

- TUI startup loop and `interactive_ready`
- agent session finish path
- session store records and local telemetry ledger startup scan

Summary recommendation:

- Include in `session.startup` and `session.lifecycle`.
- Direct event only for fatal errors that cannot be stored locally first.

### Graph And Performance

Capture:

- cold graph build duration, status, language distribution, files seen/parsed
- ignored/excluded file/dir/byte counts
- persisted cache loaded/missed/rebuilt counts
- incremental refresh count, duration buckets, changed/parsed file buckets
- graph query tool counts by query family and status

Origin points:

- `ToolRegistry` deferred `GraphManager::open_with_store`
- graph refresh and graph-backed tool dispatch

Summary recommendation:

- Include in `session.graph`.
- Avoid per-refresh remote events except sampled diagnostics.

### Slash Commands

Capture:

- command token from canonical slash head, never arguments
- surface: TUI composer, TUI inline, agent raw
- outcome: accepted, usage error, blocked during turn, unknown, template
  expanded, error
- alias kind and argument shape only

Origin points:

- TUI slash command handler
- inline slash handler
- headless `dispatch_command_raw`

Summary recommendation:

- Aggregate counts by `(command, surface, outcome, alias_kind, arg_shape)`.
- Unknown raw heads should collapse to `unknown`.

### Config

Capture:

- scope: user, project, local, session
- section from `SectionId::slug()`
- field id from schema `toml_path`
- apply tier from `FieldMeta::tier`
- change kind: set, unset, reset
- previous/new value buckets only
- count of discarded/undone edits, if useful

Origin points:

- `/config` pane save helpers
- close-time telemetry drain
- schema metadata in `squeezy-core`

Summary recommendation:

- Aggregate by `(scope, section, field, apply_tier, change_kind, prev_bucket,
  new_bucket)`.
- Send only after committed writes survive undo/discard.

### Tools, Approvals, Permissions, And Failures

Capture:

- tool calls by first-party tool family/name and status
- duration buckets, bytes read buckets, output bytes buckets, matches buckets
- approval decisions by capability/risk/action/source, without targets/reasons
- denied/cancelled/stale/error counts by coarse kind
- sandbox fallback counts by backend
- budget gates: warn, hard cap, pressure, round input, unpriced

Origin points:

- agent tool telemetry emit path
- tool registry permission request/evaluation
- shell sandbox fallback
- cost broker gates

Summary recommendation:

- Aggregate into `session.tools`, `session.approvals`, and `session.failures`.
- Do not send raw SHA-256 of tool args/output/content remotely.

### Provider, Routing, Cache, And Retry

Capture:

- provider kind and model family only
- route kind: parent, cheap, judge, subagent, reviewer
- provider round status, stop reason bucket, reasoning-only stop boolean
- token/cost/cache counters already present in turn metrics
- cache supported, retention bucket, marker count, cached input tokens
- retry attempt buckets: request, auth refresh, stream reconnect
- retry reason buckets: rate limit, 5xx, transport, idle timeout, truncated,
  divergence, terminal quota, non-retryable
- provider error bucket: auth, permission, quota, rate limit, context overflow,
  content filter, invalid request, not found, server, transport, parse, unknown

Origin points:

- LLM request/stream loop
- retry policy
- provider usage parsers
- turn router and cost broker

Summary recommendation:

- Aggregate into `session.provider`, `session.routing`, `session.cache`, and
  `session.retry`.
- Do not send raw provider error messages, response ids, cache keys, base URLs,
  headers, profile names, or exact model ids.

## Five-Agent Audit Results

### Skills And Prompt Templates

Current coverage:

- Skills have no dedicated product telemetry.
- Prompt templates are only visible through generic slash-command telemetry for
  `/prompt-template`, with `alias_kind = template` and
  `outcome = template_expanded`.

Capture:

- skill discovery counts by source: builtin, user, project, explicit path
- disabled skill count and ambiguous-name count
- context mode counts, inline-vs-metadata-only counts, and budget mode buckets
- activation counts by explicit, trigger, and implicit shell activation
- active skills total, included count, dropped count, and body-truncated count
- `load_skill` and `list_skills` counts by success/error status
- prompt-template discovery counts by user/project source
- prompt-template expansion count, queued-vs-started count, and arg-count bucket
- render pressure: preamble emitted, preamble omitted, dropped render sections,
  and truncation counts

Origin points:

- skill discovery and registry paths in `crates/squeezy-skills/src/lib.rs`
- active-skill render path in `crates/squeezy-skills/src/render.rs`
- prompt-template discovery in
  `crates/squeezy-skills/src/prompt_templates.rs`
- TUI prompt-template dispatch in `crates/squeezy-tui/src/lib.rs`
- shell implicit-skill path in `crates/squeezy-tools/src/shell.rs`
- skill tool execution in `crates/squeezy-tools/src/lib.rs`

Privacy exclusions:

- raw skill names, prompt-template names, descriptions, `when_to_use`,
  `prompt_hint`, `tool_deps`, hook commands, skill bodies, template bodies,
  expanded prompts, slash args, shell commands, workdirs, outputs, and error
  strings containing paths or content
- content-derived hashes such as `args_sha256`, `output_sha256`, or
  `content_sha256` as remote skill/template identifiers

Recommendation:

- Add `session.skills` and `session.prompt_templates` sections to
  `squeezy_session_summary`.
- Do not add direct skill/template events except the existing generic slash
  signal while the current event model is still transitional.

### MCP And External Integrations

Execution note:

- The first follow-up agent for this domain errored during remote compaction
  before returning findings. A replacement MCP-only explorer completed, and
  its findings are incorporated with the local scan below.

Current coverage:

- MCP calls are represented as ordinary tool telemetry when a model calls an
  `mcp__...` tool.
- There is no dedicated MCP summary section.
- Web lookup/fetch are first-party tools backed by external network calls and
  should share the same privacy boundary as MCP.

Capture:

- configured MCP server count by transport bucket: stdio, http, sse
- enabled/disabled configured server counts
- per-session MCP permission mode buckets, timeout buckets, and
  tool-allowlist/tool-denylist presence counts
- discovery attempts, discovery duration buckets, and discovery status buckets
- discovered tool counts, cached tool counts, retained-stale-on-failure count,
  and dropped-disabled-server count
- cached-vs-fresh tool palette use and discovery error kind buckets
- MCP tool call count by status, duration bucket, timeout bucket, and output
  byte bucket
- coarse MCP tool dimensions such as `tool_family = mcp` and
  `external_tool = true`; current generic tool telemetry collapses unknown
  `mcp__...` tools to `other`
- MCP resource list, resource-template list, and resource-read counts by status
- resource-read cache hits, evictions, and list-change invalidations
- server capability presence booleans only: resources, tools, elicitation,
  experimental-present
- elicitation count by kind, policy, and outcome: auto accepted, auto declined,
  forwarded, user declined, user cancelled
- permission decisions for MCP by action bucket and source bucket
- provider/external integration facts: provider kind, model family,
  OpenAI-compatible preset bucket, keyless-vs-key-backed/auth mode,
  local-vs-hosted bucket, OAuth-enabled bucket, and websearch backend enum
- websearch/webfetch counts by provider enum, status, duration bucket,
  redirect-block count, internal-address-block count, cache-hit/stale count,
  and response byte bucket

Origin points:

- MCP config loading and derived permission rules in `crates/squeezy-core/src/lib.rs`
- `squeezy mcp` management commands in `crates/squeezy-cli/src/main.rs`
- MCP discovery, status snapshots, calls, resources, elicitations, and resource
  cache in `crates/squeezy-mcp/src/lib.rs`
- MCP tool exposure, permission scope, and execution in
  `crates/squeezy-tools/src/lib.rs`
- production turn refresh in `crates/squeezy-agent/src/lib.rs`
- websearch/webfetch external calls, SSRF checks, redirects, cache receipts, and
  parsing in `crates/squeezy-tools/src/web.rs`
- provider family/model-family telemetry mapping in
  `crates/squeezy-telemetry/src/lib.rs`
- provider presets, hosted/local bucket inputs, and auth-mode inputs in
  `crates/squeezy-core/src/lib.rs`

Privacy exclusions:

- server names, tool names, raw MCP model names, commands, command args, env
  variable names/values, custom headers, URLs, domains, resource URIs, resource
  contents, web queries, fetched content, fetched citations, auth-token state,
  provider error text, request/response ids, and all tool args/output
- custom external endpoint URLs, including `exa_mcp_url` and `parallel_mcp_url`
- provider API keys, custom base URLs, org/project ids, Cloudflare
  account/gateway/deployment ids, AI Gateway metadata/cache keys, Bedrock
  request metadata, Vertex project/location, OAuth credential paths, and bearer
  token env var names

Recommendation:

- Add `session.mcp` and `session.external_network` aggregate sections.
- Add Worker schema support before relying on new MCP fields; the current
  Worker drops unknown properties.
- Keep MCP/web facts in the summary by default. Direct-send only consented
  `/feedback` or `/report` paths, or fatal unrecordable failures.

### Subagents, Tools, Approvals, Permissions, And Failures

Current coverage:

- `squeezy_session_ended` includes only `subagent_calls` and
  `subagent_failures`.
- Tool telemetry is high-frequency and direct-sent as `squeezy_tool_completed`.
- Permission/approval/failure paths have some direct events but no durable
  summary model.

Capture:

- subagents by kind, status bucket, calls, failures, concurrency-cap bucket,
  child tool count, child cost bucket, and child byte buckets
- tool calls by first-party tool family/name enum, status, duration bucket,
  file count bucket, bytes read bucket, output byte bucket, and match count
- approval requests/prompts by capability, risk bucket, decision bucket,
  rule-install action, source bucket, timeout count, and cancel count
- permission decisions by capability, policy action, source bucket, session mode
  gate, AI-reviewer outcome, and pre-classifier allow/deny bucket
- failures by phase bucket, coarse error kind, panic boolean, tool status, graph
  status, and cancellation phase
- budget gates by gate kind, denial count, cap/limit bucket, percent bucket,
  unpriced-model count, and pre/post/pressure/round-input bucket

Origin points:

- subagent dispatch and accounting in `crates/squeezy-agent/src/lib.rs`
- tool execution and progress telemetry in `crates/squeezy-agent/src/lib.rs`
- approval logs and permission decisions in `crates/squeezy-agent/src/lib.rs`
- policy evaluation in `crates/squeezy-core/src/lib.rs`
- AI reviewer downgrade path in `crates/squeezy-agent/src/ai_reviewer.rs`
- cost broker gates in `crates/squeezy-agent/src/cost_broker.rs`

Privacy exclusions:

- subagent prompts, summaries, transcripts, files touched, receipts, raw model
  ids, raw error text, tool args, shell commands, paths, URLs, output snippets,
  provider response text, stack traces, matched permission-rule targets, and
  content-derived hashes

Recommendation:

- Move high-frequency tool and subagent facts into `session.tools`,
  `session.subagents`, `session.approvals`, `session.permissions`,
  `session.failures`, `session.cancellations`, and `session.budgets`.
- Keep only sampled diagnostics or fatal unrecordable failures as direct events.

### Session, Startup, Config, Graph, And Performance

Current coverage gaps:

- The code is still event-based; `squeezy_session_summary` is only planned in
  internal docs.
- `squeezy_graph_refresh_completed` exists in schema/Worker/docs, but no
  emitter was found. Refreshes happen through `GraphManager::refresh_before_query`.
- `squeezy_startup_ready` and `squeezy_session_ended` are TUI-normal-exit
  oriented. Prompt/headless mode builds an agent and pumps turns but does not
  call `finish_session`.
- `TelemetryClient::begin_turn` and `TelemetryClient::end_turn` exist, but no
  production code calls them, so only session-level trace ids are stamped.
- Tool telemetry currently sends `args_sha256`, `output_sha256`, and
  `content_sha256`, which conflicts with the summary privacy boundary.

Capture:

- session lifecycle: app start, clean exit, abnormal exit, duration, final
  status, turn count, prompt queue count, job count, pending-summary drain
  attempts, and drain outcomes
- startup performance: route, launch-to-placeholder draw, agent build,
  snapshot load, first interactive draw, and route classifier result
- config changes: scope, section slug, schema field id, apply tier, set/unset
  or reset kind, previous value bucket, and new value bucket
- graph build/refresh: duration, status, cold/incremental flag, file/language
  counts, excluded file/dir/byte counts, persisted cache loaded/missed/rebuilt
  counts, symbols, edges, refresh counts, and refresh duration buckets
- general performance: turn/tool counts, status, duration buckets, files
  scanned, bytes read, output bytes, matches, tokens, cached tokens, estimated
  cost, routing counters, and retry counters

Origin points:

- app-start telemetry in `crates/squeezy-cli/src/main.rs`
- TUI first-interactive startup telemetry and session finish in
  `crates/squeezy-tui/src/lib.rs`
- agent session finish, turn metrics, and tool telemetry in
  `crates/squeezy-agent/src/lib.rs`
- config save/report drain in `crates/squeezy-tui/src/config_screen/save.rs`
  and `crates/squeezy-tui/src/lib.rs`
- graph build telemetry in `crates/squeezy-tools/src/lib.rs`
- graph refresh reports in `crates/squeezy-graph/src/lib.rs`
- Worker allowlist and sanitization in `infra/telemetry-worker/src/worker.ts`

Privacy exclusions:

- prompts, transcript labels, cwd, repo names, resumed session ids, exact model
  ids, raw settings, endpoint URLs, paths, secrets, custom model ids, config
  list contents, file paths, symbol names, declaration names, and all remote raw
  SHA fields

Recommendation:

- Make this the backbone of `squeezy_session_summary`.
- Fill prompt/headless `finish_session` and graph-refresh recording gaps before
  relying on summary completeness.
- Remove raw SHA fields from remote telemetry before or during the summary
  migration.

### Persistence, Retry, Worker, And PostHog Architecture

Current coverage:

- `TelemetryClient` currently buffers in memory, flushes at 50 events or a
  five-second interval, and drops failed sends.
- CLI exit only waits briefly for the top-level client.
- A normal run can create two telemetry clients: one in CLI startup and one in
  `Agent::build`.
- Worker caps are currently 64 KiB request bodies and 50 events per batch. A
  raw nested session event log can exceed that quickly; a bounded aggregate
  summary should target a much smaller size.

Recommended durable store:

- Reuse `state.redb` and existing `SqueezyStore` patterns instead of adding an
  events JSONL file or writing into graph storage.
- Add `telemetry_records`, keyed by `session_id + sequence`, with
  `{schema_version, session_id, occurred_at_ms, sequence, kind, payload}`.
- Add `telemetry_sessions`, keyed by `session_id`, with
  `{started_at_ms, last_seen_at_ms, ended_at_ms, status, abnormal_exit,
  summarized_at_ms}`.
- Add `pending_telemetry_summaries`, keyed by summary id, with
  `{created_at_ms, attempts, next_attempt_at_ms, endpoint, batch}`.
- Optionally add short-TTL sent-summary tombstones for duplicate suppression.

Flush and retry model:

- On startup, drain pending summaries before current-session telemetry.
- On normal end, write a clean end record, reduce records sorted by timestamp
  and sequence, store a pending summary, then attempt a short send.
- On startup, synthesize abnormal summaries for stale sessions with no clean end.
- Retry 5xx, network errors, and timeouts with exponential backoff plus jitter.
- For 400/413, mark rejected or reduce once more and retry once.
- Use `summary_id` as PostHog `$insert_id` to make retries idempotent.

Caps:

- local records: roughly 2 KiB each, max 5,000 records or 1 MiB per session
  before rolling into aggregate-only mode with `truncated = true`
- pending summaries: max 100 or 30 days before dropping oldest
- remote summary target: prefer less than 16 KiB; never depend on a 20 MiB
  remote payload budget

Privacy exclusions:

- prompts, responses, paths, repo/branch/labels, session titles, raw settings,
  shell commands/output, URLs, env values, exact model ids, tool args, provider
  response ids, cache keys, opaque content hashes, and endpoint domains

Recommendation:

- Implement one shared telemetry runtime/session id per process.
- Send one `squeezy_session_summary` event per session by default.
- Keep `/feedback` and `/report` as separate direct-send flows with explicit
  user consent.
