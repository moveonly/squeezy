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
  Git blob hashes (`before_sha256` / `after_sha256`), raw worktree byte hashes
  (`before_worktree_sha256` / `after_worktree_sha256`), POSIX file type, Git
  mode, symlink targets when the tree entry is a symlink, and Unix hardlink
  peer paths when a changed regular file belongs to a link group.
- `files`, `skipped_files`, `summary`, `coverage_warnings`, and
  `created_at_ms`.

`RollbackTarget` (same file) is therefore three-way:

- `Latest` — the most recent record that has not already been fully rolled
  back by a successful rollback journal entry.
- `Group(group_id)` — every record tagged with that turn id.
- `Checkpoint(id)` — one specific record.

Both `Atomic` and `BestEffort` rollback modes accept any of the three
targets; the sha256 and file-type gates are identical in all cases. `Atomic`
writes a backup of every touched path to memory before applying so a per-file
write failure rolls the workspace back to the pre-rollback bytes — atomic apart
from a crash window between apply and restore. A process crash between the
partial apply and either successful completion or backup restore can leave the
workspace half-applied with no on-disk recovery state.

`checkpoint_undo` targets `Latest`; `checkpoint_revert` requires exactly one of
`group_id` or `checkpoint_id`. Both tool surfaces preflight the full set of
paths that rollback can mutate, including the source side of a reversed rename
and hardlink peers. `checkpoint_restore_file` uses the same conflict and path
safety gates but restores only one file and does not mark the whole checkpoint
consumed for future `Latest` undo selection.

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

Rollback is tree-entry aware on Unix-like platforms. Regular files are
restored through a sibling tempfile, file fsync, atomic rename, and parent-dir
fsync. Git executable modes are reapplied from the before tree. Symlink entries
are restored as symlinks from the Git symlink blob instead of by writing the
target text into a regular file, and conflict preflight hashes the symlink
target without following it. Deletes unlink the workspace entry itself and
refuse to remove directories. Hardlinked regular files are restored to the
before snapshot and then relinked so the recorded link group shares one inode;
unchanged peers are preflighted before relinking so rollback does not overwrite
user edits made after the checkpoint. The relink itself is crash-atomic: a
sibling tempfile is hard-linked from the source inode and renamed over the
peer, so an interrupt between the unlink and link cannot leave the peer
absent. The rollback receipt includes a per-file `file_actions` list with
`restore_regular`, `restore_symlink`, `restore_hardlink`, or `delete`, the
mode/file type when relevant, and a `verified_after_rollback` boolean.
Restores fail if post-rollback verification does not match the expected
content hash, file type, Git mode, or hardlink identity.

Rollback fidelity is limited to the Git tree model: the 9-bit Unix mode is
reapplied, but extended attributes (`security.capability`, SELinux contexts,
POSIX ACLs, user xattrs) are *not* preserved across a checkpoint cycle.
Restoring a `setcap`'d binary, an SELinux-labeled config, or a file with a
custom xattr will produce content-identical bytes but a stripped security
envelope. Workspaces relying on those attributes should treat checkpoint
rollback as a content-only restore and reapply labels/capabilities through
the same tooling that set them.

Checkpoint coverage is explicit. Files that exceed the checkpoint size limit
are reported in `skipped_files` and add a coverage warning; rollback will not
pretend those files are protected. Malformed journal lines are counted as
`journal_warnings` and ignored rather than making the whole checkpoint list
unreadable.

Large-file discovery is Git-driven: the shadow repo asks `git ls-files -z
--cached --others --exclude-standard` for eligible paths and then applies local
metadata checks. This avoids recursive per-path `git check-ignore` calls on
large Linux workspaces.

The store options are configured from `[tools]`: `checkpoint_retention_days`,
`checkpoint_max_file_bytes`, and `checkpoint_cleanup_interval_secs`. Cleanup is
throttled by the interval so ordinary edit checkpoints do not synchronously
rewrite the journal and run `git gc` on every mutation.

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

Rollback path preflight returns every path a rollback may write or delete. For
renames that includes both the destination path and the restored source path,
so current workspace write policy is checked before any rollback target can
recreate a source file. For hardlinked files, preflight also includes peer
paths that relinking may mutate.

`checkpoint_doctor` (also reachable from `/checkpoints doctor`) performs a
no-op shadow snapshot and reports the normalized workspace/shadow paths, Git
path mode, relevant shadow Git config, discovered `.gitattributes`, lock-file
writability, protected-ref create/delete capability, and a temporary CRLF
checkpoint/mutate/rollback smoke result. The smoke workspace is created outside
the user's project and removed after the report. `checkpoint_check` reports
journal/ref/blob integrity without running the smoke.

## What This Document Is Not

This is not a user manual. The agent-visible tools (`checkpoint_list`,
`checkpoint_doctor`, `checkpoint_show`, `checkpoint_undo`,
`checkpoint_revert`, `checkpoint_restore_file`, `checkpoint_check`) are
documented in their own specs under `crates/squeezy-tools/src/`. This
file records *why* the `group_id` axis exists so a future refactor
does not pare it down to `Latest` + `Checkpoint(id)` on the grounds
that `Group` looks redundant.
