# Checkpoints And Rollback

Checkpoint-enabled edit tools capture a pre-edit snapshot, run the mutation,
and record the delta as a `CheckpointRecord` in the shadow git repo under
`.squeezy/checkpoints/git/` plus a JSONL journal entry. The default bridge is
`JournalCheckpointProvider`; external integrations can replace the pre/post
edit snapshot provider, but the built-in list/show/undo/revert tools still read
the registry's own `CheckpointStore`. The agent can roll back individual
records, but the unit of work users actually care about is the **turn**, and
the rollback surface is shaped to match.

## Records, Groups, And Targets

A `CheckpointRecord` (`crates/squeezy-vcs/src/lib.rs`) carries:

- `id` — the record's own identifier.
- `group_id` — the agent turn that produced the write. Every record
  emitted while handling one user turn shares this value.
- `tool_name`, `call_id`, `status`, `before_tree`, `after_tree`, per-file
  `before_sha256` / `after_sha256`.
- `files`, `skipped_files`, `summary`, `coverage_warnings`, and
  `created_at_ms`.

`RollbackTarget` (same file) is therefore three-way:

- `Latest` — the most recent record that has not already been fully rolled
  back by a successful rollback journal entry.
- `Group(group_id)` — every record tagged with that turn id.
- `Checkpoint(id)` — one specific record.

Both `Atomic` and `BestEffort` rollback modes accept any of the three
targets; the sha256 gate is identical in all cases. `Atomic` writes a
backup of every touched path to memory before applying so a per-file
write failure rolls the workspace back to the pre-rollback bytes —
atomic apart from a crash window between apply and restore. A process
crash between the partial apply and either successful completion or
backup restore can leave the workspace half-applied with no on-disk
recovery state. `checkpoint_undo` targets
`Latest`; `checkpoint_revert` requires exactly one of `group_id` or
`checkpoint_id`. `checkpoint_restore_file` uses the same sha256 conflict gate
but restores only one file and does not mark the whole checkpoint consumed for
future `Latest` undo selection.

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

## Design Rationale

Per-call edit deltas without a group identifier force a turn-level
revert to be hand-composed from reverse hunks. The `group_id` shape
closes that gap by giving every checkpoint a single addressable unit
that can be rolled back atomically.

The shadow-repo isolation (`refs/squeezy/checkpoints/<id>/{before,after}`,
hooks/gpg/commit-graph disabled) is what makes group rollback durable
across `git gc` and invisible to the user's reflog. See
`crates/squeezy-vcs/src/lib.rs` `ensure_shadow_repo` for the rationale.

Checkpoint coverage is explicit. Files that exceed the checkpoint size limit
are reported in `skipped_files` and add a coverage warning; rollback will not
pretend those files are protected. Malformed journal lines are counted as
`journal_warnings` and ignored rather than making the whole checkpoint list
unreadable.

The store options are configured from `[tools]`: `checkpoint_retention_days`,
`checkpoint_max_file_bytes`, and `checkpoint_cleanup_interval_secs`. Cleanup is
throttled by the interval so ordinary edit checkpoints do not synchronously
rewrite the journal and run `git gc` on every mutation.

Rollback path preflight returns every path a rollback may write or delete. For
renames that includes both the destination path and the restored source path,
so current workspace write policy is checked before any rollback target can
recreate a source file.

## What This Document Is Not

This is not a user manual. The agent-visible tools (`checkpoint_list`,
`checkpoint_show`, `checkpoint_undo`, `checkpoint_revert`,
`checkpoint_restore_file`, `checkpoint_check`) are documented in their own
specs under `crates/squeezy-tools/src/`. This file records *why* the
`group_id` axis exists so a future refactor does not pare it down to `Latest`
+ `Checkpoint(id)` on the grounds that `Group` looks redundant.
