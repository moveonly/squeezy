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

## Startup Critical Path (time-to-interactive)

The prompt must accept keystrokes before any cosmetic or maintenance work
finishes. The boundary is: only what the first frame and the first turn's
correctness depend on runs synchronously between process start and the main
loop's first `draw_app`; everything else is deferred to the blocking pool and
folded in once it lands.

Deferred off the boot path:

- **Workspace graph open** — `GraphManager::open_with_store` (tree-sitter init +
  redb hydrate) runs in a `spawn_blocking` task; graph tool calls wait on
  `graph_ready` up to `GRAPH_READY_WAIT`. `graph.redb` is opened lazily there,
  never eagerly at startup.
- **Plan housekeeping** — legacy migration, the 30-day `git log` protected-id
  scan, and plan-dir pruning run in a `spawn_blocking` task; results arrive as
  log lines via `drain_plan_housekeeping`.
- **Repo status probe** — the status-bar branch/changed-files/PR-number/branch-
  diff is built from a git worktree snapshot, a `gh pr view` network call, and a
  `git diff --shortstat`. These dominated time-to-interactive, so
  `RepoStatus::detect_at` runs in a `spawn_blocking` task; the status bar shows
  a neutral `…` placeholder (`RepoStatus::pending()`) until `drain_repo_status`
  installs the result.
- **Compaction-checkpoint GC** — `prune_compaction_checkpoints` is a best-effort
  redb write transaction with no input-path dependency; it is handed to the
  blocking pool when a runtime is present.

Kept synchronous but parallelized: `detect_git_state` fans its four independent
git probes (`branch`, `rev-parse HEAD`, `symbolic-ref origin/HEAD`, `status
--porcelain`) across threads so onboarding pays the slowest probe, not their
sum. All four feed the light fingerprint, so none can be dropped.

Measure with `SQUEEZY_STARTUP_TRACE_FILE=/path` (see
`squeezy_core::startup_trace`): each milestone from `main_start` to
`interactive_ready` is appended as `"<elapsed_micros> <label>"`. With the env
var unset every `mark` is one atomic load plus a branch, so the calls stay on
the hot path as a regression guard.

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
