# Memory Scope

Squeezy cross-session memory is a single static file: `~/.squeezy/memory.md`,
capped by `context_compaction.user_memory_max_bytes`
(`crates/squeezy-core/src/lib.rs::ContextCompactionConfig::user_memory_max_bytes`)
and stitched into the base instructions at session start. The user curates
this file by hand; the agent has no tool that can write back to it.

Squeezy declines to ship a tool-mediated memory pipeline in the v1 graph
milestone.

Supported scope:

- Read `~/.squeezy/memory.md` once at session start.
- Apply the configured byte cap and append the trimmed body to base
  instructions.
- Let the user edit the file out-of-band; no in-session reload.

Out of scope (deferred):

- A `memory_append` tool the model can call to record cross-session facts.
- A background per-rollout extraction phase that proposes memory entries.
- A second consolidation phase that merges or rewrites stored memories.
- Per-project or per-thread memory partitions; the single user-global file
  is the only durable store.
- Automatic injection of memories beyond the single file ingestion path.

If a future user request justifies adoption, the staged path is:

1. Expose `memory_append` as a write-capability tool gated on user
   approval. Each call persists one line to a designated section of the
   user-global memory file. The existing approval and audit surfaces cover
   the trust boundary; no separate hook framework is introduced.
2. Once approved appends accumulate, add a deterministic "recent decisions"
   section the agent writes to, so layout stays predictable for the
   ingestion cap.
3. Only after real usage data justifies the cost, consider a phase-2
   consolidation pass that rewrites or deduplicates entries.

Keep durable cross-session knowledge in the static memory file or in
`bd remember` (Beads) until step 1 lands. Skills, hooks, and remote
runtimes are not part of the memory path; see `SKILLS_SCOPE.md` for the
adjacent decision on instruction bundles.
