# Memory Scope

Squeezy has two separate cross-session memory surfaces today:

- **Startup instruction memory**: `squeezy-agent` reads
  `~/.squeezy/MEMORY.md` first, then lowercase `~/.squeezy/memory.md`, once at
  session start. The body is capped by
  `context_compaction.user_memory_max_bytes` and appended to the base
  instructions. A value of `0` disables ingestion. Missing, empty, unreadable,
  or `$HOME`-less cases are silent best-effort skips.
- **Store-backed observations**: `notes_remember`, `notes_recall`, and
  `observations` persist and query typed observations in the local `redb`
  state store. Observation kinds are `preference`, `decision`, `convention`,
  `dead_end`, and `note`. Recent observations are also summarized during
  context compaction so durable decisions are not silently dropped from a
  compacted turn.

These surfaces are intentionally not the same store. The startup memory file is
model-visible preamble that the user can curate directly. Store-backed
observations are model-callable tools backed by structured local state; they do
not rewrite `MEMORY.md` and are not injected wholesale into every session.

## Implemented Scope

- Read `~/.squeezy/MEMORY.md` or `~/.squeezy/memory.md` once at session start
  through `ingest_user_memory`.
- Truncate startup memory at a UTF-8 character boundary and append
  `"[truncated]"` when capped.
- Keep `SessionStore::remember` / `SessionStore::recall` as the canonical
  line-oriented helper for the lowercase `memory.md` file. It appends trimmed
  lines, preserves one-entry-per-line shape, and returns `None`/errors when
  `$HOME` is unavailable.
- Expose structured note persistence through the tool registry:
  `notes_remember` writes one observation, `notes_recall` searches or lists
  recent matches, and `observations` provides a read-only listing/search view.

## Out Of Scope

- A separate `memory_append` tool name or an automatic write-back path from
  observations into `MEMORY.md`.
- Background extraction that proposes memories after every rollout.
- Automatic consolidation that rewrites or deduplicates either `MEMORY.md` or
  the observation store.
- Remote memory services, plugin-owned memory stores, or hook-driven hidden
  memory mutation.
- Treating store-backed observations as a replacement for the explicit startup
  memory file.

Skills, hooks, and remote runtimes are not part of the startup memory path; see
`SKILLS_SCOPE.md` for the adjacent decision on local instruction bundles.
