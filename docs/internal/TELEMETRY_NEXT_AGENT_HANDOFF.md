# Telemetry Next-Agent Handoff

This branch migrates high-frequency product telemetry into one bounded
`squeezy_session_summary` event built from a local durable telemetry ledger.

## Current Implementation

- Existing event constructors remain as local safe fact records. Call sites can
  still record startup, session end, graph, slash, config, tool, routing, and
  coarse failure facts without knowing about the remote summary.
- `squeezy_session_summary` is the remote product telemetry event. It carries
  aggregate counters plus capped count maps for already-sanitized tool, slash,
  failure, routing, and config tokens.
- The telemetry client buffers safely when called from sync code without a
  Tokio runtime.
- The Worker accepts product event names that match `squeezy_*`, then forwards
  only bounded safe properties: non-negative counters, booleans, token strings,
  and small count maps. This keeps the Worker forward-compatible with future
  summary fields without forwarding raw text, paths, URLs, arrays, or arbitrary
  nested objects.

## One-Event Summary Direction

Local records are the source of truth and PostHog is the aggregate sink:

1. Persist a local telemetry record at the moment a safe fact happens.
2. Include `occurred_at_ms` and a monotonic local sequence on each record.
3. On session exit, sort records by `(occurred_at_ms, sequence)` and reduce them
   into one bounded `squeezy_session_summary` event.
4. Store the summary as pending before sending.
5. If sending fails, retry pending summaries on next session start before new
   telemetry is sent.
6. On startup, detect prior sessions that have a start record but no clean end
   record and synthesize a summary with an abnormal status.
7. Delete pending summaries and their source records only after the Worker
   returns success.

The raw local ledger can be more detailed than PostHog, but the remote summary
must stay bounded and aggregate-first.

## Exclusions From The Single Summary

Keep these out of the one summary event:

- Explicit user-consented flows: `/feedback` and `/report` should keep their
  separate direct endpoints and preview/redaction behavior.
- Raw user content: prompts, model responses, file contents, snippets, shell
  commands, command output, URLs, environment values, API keys, and raw settings
  values.
- Raw paths, repository names, session titles, labels, custom model ids, template
  names, slash arguments, tool arguments, or opaque hashes that can fingerprint
  user content.
- Unbounded nested event arrays. If a section grows beyond a cap, summarize the
  top buckets and include `truncated = true` plus dropped counts.
- Full local diagnostic timelines by default. If needed, add opt-in diagnostic
  upload or low-rate sampling later.

Candidate direct-send exceptions besides feedback/report are only fatal errors
that cannot be durably recorded first. Prefer durable local recording and
next-start recovery whenever possible.

## Suggested Summary Sections

- Session: started/ended timestamps, duration, status, abnormal-exit flag.
- Startup: route, time to placeholder draw, agent build, first interactive draw.
- Graph: build/refresh counts, duration buckets, file/language/exclusion/cache
  counts, error counts.
- Slash usage: counts by command token, surface, outcome, alias kind, arg shape.
- Config: counts by scope, section, field id, apply tier, change kind, value
  bucket transition.
- Tools: counts by tool family/name/status, duration buckets, bytes/read/search
  buckets, output buckets.
- Failures: counts by coarse error kind and phase.
- Cost/context: aggregate token/cost/cache/budget counters already captured in
  turn/session metrics.

## Files Touched In This Branch

- `crates/squeezy-telemetry/src/lib.rs`
- `crates/squeezy-telemetry/src/lib_tests.rs`
- `crates/squeezy-agent/src/lib.rs`
- `crates/squeezy-tools/src/lib.rs`
- `crates/squeezy-tools/Cargo.toml`
- `crates/squeezy-tui/src/lib.rs`
- `crates/squeezy-tui/src/config_screen.rs`
- `crates/squeezy-tui/src/config_screen/keys.rs`
- `crates/squeezy-tui/src/config_screen/save.rs`
- `infra/telemetry-worker/src/worker.ts`
- `infra/telemetry-worker/tests/worker.test.ts`
- `crates/squeezy-skills/external-docs/TELEMETRY.md`

## Validation Run

- `cargo test -p squeezy-telemetry`
- `cargo check -p squeezy-agent -p squeezy-tui -p squeezy`
