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
  `before_sha256` / `after_sha256`, POSIX file type, Git mode, symlink
  targets when the tree entry is a symlink, and Unix hardlink peer paths when a
  changed regular file belongs to a link group.
- `files`, `skipped_files`, `summary`, `coverage_warnings`, and
  `created_at_ms`.

`RollbackTarget` (same file) is therefore three-way:

- `Latest` — the most recent record.
- `Group(group_id)` — every record tagged with that turn id.
- `Checkpoint(id)` — one specific record.

Both `Atomic` and `BestEffort` rollback modes accept any of the three
targets; the sha256 and file-type gates are identical in all cases.
`checkpoint_undo` targets `Latest`; `checkpoint_revert` requires exactly one
of `group_id` or `checkpoint_id`. Both tool surfaces preflight the full set of
paths that rollback can mutate, including the source side of a reversed rename.

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

Rollback is tree-entry aware on Unix-like platforms. Regular files are
restored through a sibling tempfile, file fsync, atomic rename, and parent-dir
fsync. Git executable modes are reapplied from the before tree. Symlink entries
are restored as symlinks from the Git symlink blob instead of by writing the
target text into a regular file, and conflict preflight hashes the symlink
target without following it. Deletes unlink the workspace entry itself and
refuse to remove directories. Hardlinked regular files are restored to the
before snapshot and then relinked so the recorded link group shares one inode;
unchanged peers are preflighted before relinking so rollback does not overwrite
user edits made after the checkpoint. The rollback receipt includes a per-file
`file_actions` list with `restore_regular`, `restore_symlink`,
`restore_hardlink`, or `delete`, the mode/file type when relevant, and a
`verified_after_rollback` boolean. Restores fail if post-rollback verification
does not match the expected content hash, file type, Git mode, or hardlink
identity.

Checkpoint coverage is explicit. Files that exceed the checkpoint size limit
are reported in `skipped_files` and add a coverage warning; rollback will not
pretend those files are protected. Malformed journal lines are counted as
`journal_warnings` and ignored rather than making the whole checkpoint list
unreadable.

Large-file discovery is Git-driven: the shadow repo asks `git ls-files -z
--cached --others --exclude-standard` for eligible paths and then applies local
metadata checks. This avoids recursive per-path `git check-ignore` calls on
large Linux workspaces.

## What This Document Is Not

This is not a user manual. The agent-visible tools (`checkpoint_list`,
`checkpoint_show`, `checkpoint_undo`, `checkpoint_revert`) are
documented in their own specs under `crates/squeezy-tools/src/`. This
file records *why* the `group_id` axis exists so a future refactor
does not pare it down to `Latest` + `Checkpoint(id)` on the grounds
that `Group` looks redundant.
