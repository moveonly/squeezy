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
  Git blob hashes (`before_sha256` / `after_sha256`) plus raw worktree byte
  hashes (`before_worktree_sha256` / `after_worktree_sha256`).
- `files`, `skipped_files`, `summary`, `coverage_warnings`, and
  `created_at_ms`.

`RollbackTarget` (same file) is therefore three-way:

- `Latest` — the most recent record.
- `Group(group_id)` — every record tagged with that turn id.
- `Checkpoint(id)` — one specific record.

Both `Atomic` and `BestEffort` rollback modes accept any of the three
targets; the sha256 gate is identical in all cases. `checkpoint_undo` targets
`Latest`; `checkpoint_revert` requires exactly one of `group_id` or
`checkpoint_id`.

Rollback safety is checked against the raw worktree byte hash when the
checkpoint recorded one. Git blob hashes remain in the record for object
integrity and display diagnostics, but they are not used as the only safety
gate because `.gitattributes`, CRLF conversion, and clean filters can make a
Git blob differ from the bytes a Windows editor actually left on disk.
Checkpoints created before this change have no
`before_worktree_sha256` / `after_worktree_sha256` field and continue to
use the Git blob hash as their only safety gate; run a fresh checkpoint
through the agent to get raw-byte safety.

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

Rollback attempts are journaled even when only some paths are restored.
`BestEffort` mode converts per-file filesystem errors into structured
conflicts and continues with later files. `Atomic` mode preflights
read-only attributes and write-handle availability before mutating, so
many obvious conflicts surface before any byte is touched. The probe
opens each planned-write target with the OS's default share mode, so an
exclusive-share editor lock (e.g. a legacy editor that opens with
`dwShareMode = 0`, or an antivirus scanner) can still slip past the
preflight and produce a per-file conflict during the actual rollback,
which is then journaled as an applied-with-conflicts row. This matters
on Windows, where read-only attributes, editor locks, antivirus
scanners, and sync tools can reject one write/delete while other paths
are still safe.

`checkpoint_doctor` (also reachable from `/checkpoints doctor`) performs a
no-op shadow snapshot and reports the normalized workspace/shadow paths, Git
path mode, relevant shadow Git config, discovered `.gitattributes`, lock-file
writability, protected-ref create/delete capability, and a temporary CRLF
checkpoint/mutate/rollback smoke result. The smoke workspace is created outside
the user's project and removed after the report.

## What This Document Is Not

This is not a user manual. The agent-visible tools (`checkpoint_list`,
`checkpoint_doctor`, `checkpoint_show`, `checkpoint_undo`,
`checkpoint_revert`) are
documented in their own specs under `crates/squeezy-tools/src/`. This
file records *why* the `group_id` axis exists so a future refactor
does not pare it down to `Latest` + `Checkpoint(id)` on the grounds
that `Group` looks redundant.
