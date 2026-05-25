# Async And Background Work

This document records the current local-only background-work boundary. It is a
guardrail for implementation work: do not add durable queues, file watchers, or
telemetry spools unless a concrete producer creates a measured need.

## Current Shape

- Agent turns and local tool jobs are in-process Tokio tasks.
- Tracked jobs keep a cancellation token plus an abort handle. Cancellation is
  cooperative first, then hard-aborted after the grace window if the task does
  not drain.
- Session event writes are off the async runtime. `SessionHandle::append_event`
  queues JSONL payloads to a per-session writer thread; session boundaries and
  read surfaces use `flush_events()` as the durability barrier.
- Telemetry remains anonymous, in-memory, batched, and best-effort. Normal
  CLI/TUI exit calls `flush()`.
- Graph freshness remains poll-on-query plus explicit invalidation from tools
  that know they changed files.

## Deferred Durable Task Queue

Do not add a `pending_jobs` table or redb-backed local queue speculatively.
The current job registry is process-local by design. A durable queue needs all
of these before it is worth the schema and UX cost:

- a concrete producer that must survive process exit,
- idempotent replay semantics,
- retry and backoff policy,
- startup UX for resuming or discarding old work,
- migration and cleanup rules for persisted queue records.

Candidate triggers are a graph rebuild that becomes user-visible after a schema
bump, compaction work that intentionally outlives a turn, or a future telemetry
spool that earns persistence.

## Deferred Workspace File Watcher

Do not add `notify`, inotify, or FSEvents just to keep the graph warm. The
current `GraphManager` refresh policy already refreshes before graph queries,
debounces explicit changed paths, and has a bounded polling fallback.

Revisit a watcher only when there is a measured stale-result problem from
outside-Squeezy edits or a live diagnostic UI that requires sub-second
freshness. If that happens, keep the poll-on-query path as the fallback and
coalesce mutating events before they reach the graph refresh queue.

## Deferred Telemetry Spool

Do not persist automatic telemetry by default. The telemetry contract is
anonymous, cheap, and best-effort. A `telemetry_spool.jsonl` would add a write
per event plus startup replay logic for a small gain unless dashboards show
meaningful crash-related loss.

If persistence becomes justified, reuse the session-log writer pattern rather
than introducing a second concurrent-writer model.
