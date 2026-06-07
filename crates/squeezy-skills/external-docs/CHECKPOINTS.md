# Checkpoints

Squeezy can create local checkpoints around mutating tools so recent agent changes can be inspected, undone, or reverted without relying on the user's primary Git history.

Checkpointing is disabled by default. To opt in, set `checkpoints_enabled = true` under `[tools]` in Squeezy settings, or set `SQUEEZY_CHECKPOINTS_ENABLED=true` in the environment. When checkpointing is disabled, mutating tools still run normally but do not attach `checkpoint` metadata, and checkpoint commands report that checkpointing is disabled.

Checkpoint retention and the large-file threshold are configurable under `[tools]`:

- `checkpoint_retention_days` controls how long journal entries and shadow Git refs are kept. The default is 7.
- `checkpoint_max_file_bytes` controls the largest file stored in checkpoint trees. The default is 2 MiB.
- `checkpoint_cleanup_interval_secs` throttles retention cleanup checks. The default is 3600 seconds; set it to 0 to run cleanup whenever the store opens or creates a checkpoint.

Checkpoint state is stored under `.squeezy/checkpoints/` inside the workspace. The shadow Git repository stores before/after trees and `journal.jsonl` stores checkpoint metadata. Checkpoint refs keep those trees reachable until retention cleanup removes them.

## Protected Tools

Checkpoints are attached to mutating local tools:

- `write_file`
- `shell`
- `apply_patch`
- `notebook_edit`

When checkpointing is enabled, read-only tools do not create checkpoints. A tool call that leaves the workspace unchanged does not create a checkpoint. `apply_patch` attaches a checkpoint for both successful applies and partial-failure errors, so a multi-file patch that fails after the first write is still recoverable via `checkpoint_undo`.

## Inspecting Checkpoints

Use `checkpoint_list` to list recent checkpoints. The response includes `journal_warnings` when malformed journal lines were ignored during recovery.

Use `checkpoint_show` with a `checkpoint_id` to inspect one checkpoint, including file paths, status, hashes, patch text for text files, binary markers, skipped files, and coverage warnings.

When checkpointing is enabled, the TUI also surfaces checkpoint commands:

- `/checkpoints` lists checkpoints.
- `/checkpoint <checkpoint_id>` shows one checkpoint.
- `/undo` undoes the latest unreverted checkpoint.
- `/revert-turn <group_id>` reverts all checkpoints in one turn or tool group.

Checkpoint slash commands remain visible when checkpointing is disabled so the opt-in path is discoverable; running them returns the disabled-checkpoints message instead of mutating files.

When a mutation tool creates a checkpoint, the `checkpoint` field attached to the tool result already includes `skipped_files` and `coverage_warnings` when present, so the agent sees rollback coverage problems inline without needing a follow-up `checkpoint_show`.

A tool call that does not change any tracked or large workspace files does not create a checkpoint, so `checkpoint_undo` always refers to the most recent real workspace mutation rather than a no-op tool call.

## Undo And Revert

Use `checkpoint_undo` to roll back the latest unreverted checkpoint. After a successful undo, a repeated latest undo returns a skipped "nothing to undo" result instead of trying to roll back the same checkpoint again.

Use `checkpoint_revert` with exactly one of:

- `group_id` to revert all checkpoints from one turn or tool group.
- `checkpoint_id` to revert one checkpoint.

Use `checkpoint_restore_file` with `checkpoint_id` and `path` to restore one file from a checkpoint without reverting the whole checkpoint or consuming it for future latest undo selection.

Use `checkpoint_check` to scan the checkpoint journal, shadow refs, and protected blobs without changing workspace files.

Rollback responses include:

- `mode`: rollback mode used.
- `planned_files`: number of protected files considered for rollback.
- `restored_files`: files restored to their previous content.
- `deleted_files`: files removed because they were added by the checkpoint.
- `conflicts`: files left untouched because the current content no longer matches the checkpoint's after-hash, or because required checkpoint objects are missing.
- `applied`: whether any rollback writes were attempted.
- `skipped`: whether no matching checkpoint was found.

## Rollback Modes

Rollback defaults to `atomic`.

`atomic` preflights every protected file in the selected checkpoint set. If any conflict is found, no file is changed.

`best_effort` restores clean files and leaves conflicting files untouched. Conflicts are still reported and the tool returns a stale result so the caller can decide what to do next.

Grouped rollbacks are applied in reverse checkpoint order, so a sequence of agent edits to the same file can be reverted back to the state before the group.

## Large And Binary Files

Binary files at or below the checkpoint size limit are restorable, but their patch text is omitted.

Files larger than the checkpoint size limit are not stored in checkpoint trees. The default limit is 2 MiB. They are reported in `skipped_files`, and the checkpoint includes a `coverage_warnings` entry. Rollback will not restore skipped large files.

## Shell Coverage Warnings

Checkpoints only protect files inside the workspace. Shell commands can still mutate paths outside the workspace. Squeezy adds a coverage warning for obvious mutating shell commands that reference absolute paths or parent-directory traversal, such as `touch /tmp/file` or `rm ../file`.

The warning is advisory. It does not block the command and it does not make outside-workspace files restorable.

## Retention And Recovery

Checkpoint retention defaults to 7 days. Cleanup removes expired checkpoint journal entries and deletes their shadow Git refs, then prunes unreachable shadow Git objects.

Journal recovery ignores malformed JSONL lines and counts them as warnings. Rollback treats missing required checkpoint objects as conflicts and leaves current workspace content untouched.

## Diff Visibility

The `.squeezy` directory is excluded from `diff_context` reporting so checkpoint state does not pollute the agent's view of workspace changes. If you keep user-authored files under `.squeezy`, move them somewhere else if you want them to appear in `diff_context`.

## On-Disk Secrets

Tool outputs are routed through the redactor before they reach the agent or are spilled to the on-disk tool output store. The checkpoint journal under `.squeezy/checkpoints/` and the shadow Git object store under `.squeezy/checkpoints/git/objects/` are written before redaction: they record the same patch text and blob contents that exist on disk in the workspace. Anyone with read access to `.squeezy/` can read those contents until they are pruned by retention cleanup.
