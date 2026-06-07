# Async And Background Work

This document records the current local-only background-work boundary. It is a
guardrail for implementation work: do not add durable queues, file watchers, or
telemetry spools unless a concrete producer creates a measured need.

## Current Shape

- Agent turns and local tool jobs are in-process Tokio tasks. Long-running
  local jobs are registered in `JobRegistry` with a `JobStatus`, progress
  snapshots, a `CancellationToken`, and, when spawned, an abort handle.
- Cancellation is cooperative first. Turn and job watchdogs abort after the
  grace window if the task does not drain, then mark the turn/job cancelled
  rather than leaving an active slot behind.
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

- **Workspace graph open** — `ToolRegistry::new_with_configs_skills_and_mcp`
  defers `GraphManager::open_with_store` (workspace crawl + tree-sitter init +
  redb hydrate) to a `spawn_blocking` task when a Tokio runtime is present.
  Graph tool calls wait on `graph_ready` up to `GRAPH_READY_WAIT`; sync
  construction contexts keep the inline open so tests observe deterministic
  graph state. `graph.redb` is opened lazily in that task, never eagerly at
  startup.
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
`interactive_ready` is appended as `"<elapsed_micros> <label>"`. Milestones are
also stored in memory for startup telemetry. With the env var unset every
`mark` records the in-memory pair and skips the file write, so the calls stay
on the hot path as a regression guard.

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

## Workspace File Watcher Boundary

`squeezy-graph` has a cross-platform `FileWatcher` backed by
`notify-debouncer-full` and `GraphManager::open_watching`. It batches
FSEvents/inotify/ReadDirectoryChangesW notifications into
`pending_changed_paths`, which `refresh_before_query` drains on the next graph
query. The watcher is an available graph API for long-lived callers, not a
daemon or IPC surface.

The default tool registry opens the graph on the blocking pool and prefers
`GraphManager::open_watching` for long-lived sessions. If watcher startup fails,
it falls back to `GraphManager::open_with_store` and reports polling mode in
graph tool payloads. Keep poll-on-query as the fallback and coalesce mutating
events before they reach the graph refresh queue.

## Deferred Telemetry Spool

Do not persist automatic telemetry by default. The telemetry contract is
anonymous, cheap, and best-effort. A `telemetry_spool.jsonl` would add a write
per event plus startup replay logic for a small gain unless dashboards show
meaningful crash-related loss.

If persistence becomes justified, reuse the session-log writer pattern rather
than introducing a second concurrent-writer model.
