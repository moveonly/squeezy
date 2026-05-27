# Checkpoints And Rollback

Every mutating tool call writes a `CheckpointRecord` into the shadow git
repo under `.squeezy/checkpoints/git/` plus a JSONL journal entry. The
agent can roll back individual records, but the unit of work users
actually care about is the **turn**, and the rollback surface is shaped
to match.

## Records, Groups, And Targets

A `CheckpointRecord` (`crates/squeezy-vcs/src/lib.rs`) carries:

- `id` — the record's own identifier.
- `group_id` — the agent turn that produced the write. Every record
  emitted while handling one user turn shares this value.
- `tool_name`, `call_id`, `status`, `before_tree`, `after_tree`, per-file
  `before_sha256` / `after_sha256`.

`RollbackTarget` (same file) is therefore three-way:

- `Latest` — the most recent record.
- `Group(group_id)` — every record tagged with that turn id.
- `Checkpoint(id)` — one specific record.

Both `Atomic` and `BestEffort` rollback modes accept any of the three
targets; the sha256 gate is identical in all cases.

## Why `Group(group_id)` Is Load-Bearing

A single agent turn often writes more than one file: `apply_patch` over
three files, then `write_file` for a generated fixture, then a follow-up
`apply_patch` that fixes a typo it noticed mid-turn. That is five
records sharing one `group_id`.

If the turn lands badly, the user wants one cleanup action. Without
`Group`, the alternatives are:

1. Iterate `Latest` five times and hope nothing else slipped in between.
2. Issue five `Checkpoint(id)` calls after first running
   `checkpoint_list` to discover them.
3. Ask the model to compose a reverse patch — which re-introduces the
   exact mutation surface the rollback is meant to neutralise.

`checkpoint_revert group_id=<turn>` collapses that to one atomic
operation. The sha256 gate still fires per file, so any file the user
edited between the agent turn and the rollback is reported as a
conflict rather than clobbered.

## Reference Comparison

Codex's `apply_patch` records `AppliedPatchDelta` per call but exposes
no group identifier and no group-level rollback verb. Reverting an
agent turn there means hand-composing reverse hunks. That is the
specific gap `group_id` closes; keep it.

The shadow-repo isolation (`refs/squeezy/checkpoints/<id>/{before,after}`,
hooks/gpg/commit-graph disabled) is what makes group rollback durable
across `git gc` and invisible to the user's reflog. See
`crates/squeezy-vcs/src/lib.rs` `ensure_shadow_repo` for the rationale.

## What This Document Is Not

This is not a user manual. The agent-visible tools (`checkpoint_list`,
`checkpoint_show`, `checkpoint_undo`, `checkpoint_revert`) are
documented in their own specs under `crates/squeezy-tools/src/`. This
file records *why* the `group_id` axis exists so a future refactor
does not pare it down to `Latest` + `Checkpoint(id)` on the grounds
that `Group` looks redundant.
