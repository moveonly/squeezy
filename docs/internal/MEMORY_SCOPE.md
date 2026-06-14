# Memory Scope

Squeezy has two complementary cross-session memory surfaces, both on by
default.

- **File-based memory** (the primary surface): durable facts as Markdown topic
  files with YAML frontmatter (`name`, `description`, `metadata.type`), pointed
  to by a one-line `MEMORY.md` index that the agent stitches into the base
  instructions at session start (topic files are read on demand). Memory kinds
  are `user`, `feedback`, `project`, and `reference`, and the kind decides the
  **scope** deterministically — the model never picks a location:
  - `user` + `feedback` → **global** (`~/.squeezy/memory/`, shared across every
    project for this user).
  - `project` + `reference` → **project** (`<workspace>/.squeezy/memory/`, local
    to the current repository; its `.squeezy/` is auto-gitignored).

  Both indexes are stitched in, each in its own labeled subsection. Memory is
  curated two ways: the model-callable `memory` tool (`save` / `delete` /
  `list` / `read`), and an automatic extraction pass (below). The whole surface
  is gated by `context_compaction.user_memory_max_bytes` (the byte cap on the
  stitched indexes): `> 0` enables guidance + indexes + curation, `0` disables
  it. Index files are also user-editable directly.
- **Store-backed observations**: `notes_remember`, `notes_recall`, and
  `observations` persist and query typed observations in the local `redb`
  state store. Observation kinds are `preference`, `decision`, `convention`,
  `dead_end`, and `note`. Recent observations are summarized during context
  compaction so durable decisions are not silently dropped from a compacted
  turn.

These surfaces are intentionally distinct. File-based memory is the
user-facing, model-visible picture of the user and project; observations are a
structured, token-searchable local index for facts the model queries
mid-project. The `memory` tool writes topic files and the `MEMORY.md` indexes;
it does not write into the observation store, and `notes_remember` does not
write into `MEMORY.md`.

## Implemented Scope

- Stitch the standing memory guidance plus the global and project `MEMORY.md`
  indexes into the system prompt whenever memory is enabled — even when empty —
  so a first-time user is bootstrapped into the save/recall loop. The global
  index falls back to the legacy lowercase `~/.squeezy/memory.md`.
- Expose model-curated memory through the `memory` tool over
  `squeezy_store::memory::Memory`, a two-scope store that routes each `save` to
  the global or project base by the memory's type, validates slugs to stay
  inside `memory/`, and serializes the index read-modify-write with an
  `flock`-guarded lock against concurrent sessions.
- **Automatic extraction**: after a top-level turn settles (never blocking the
  user, never for subagents), a cheap auxiliary LLM pass distils durable facts
  from the new conversation slice into memory, gated to stay cheap — a real
  recorded session, a resolvable small/fast model, the
  `context_compaction.memory_auto_extract` toggle (default on), and a minimum of
  new user prose since the last pass. The inline `memory` tool is the
  explicit/override path.
- Keep `SessionStore::remember` / `SessionStore::recall` as the line-oriented
  helper for the legacy lowercase `memory.md` file.
- Expose structured note persistence through `notes_remember`, `notes_recall`,
  and `observations`.

## Out Of Scope

- Automatic consolidation that rewrites or deduplicates the observation store
  behind the model's back.
- Team / shared memory directories, remote memory services, plugin-owned
  memory stores, secret-scanned team sync, or hook-driven hidden memory
  mutation.
- Treating store-backed observations as a replacement for the file-based
  memory surface, or vice versa.

Skills, hooks, and remote runtimes are not part of the memory path; see
`SKILLS_SCOPE.md` for the adjacent decision on local instruction bundles.
