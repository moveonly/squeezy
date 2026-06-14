# Session Persistence and Memory

## Motivation

Naive session resume replays every event from scratch — every user
turn, every assistant turn, every tool output fed back through the
model so it re-derives the state it had yesterday. That is paying
for the session's input tokens a second time. A hundred-turn
debugging session can mean tens of thousands of tokens re-sent on
every resume.

Cross-session memory has the same shape: a user who every session
re-types "I prefer pinned `cargo --locked` invocations" pays the
agent to re-discover that preference indefinitely. A durable file
under `HOME` skips it.

Edit-bearing tool calls have a third version of the same pattern.
If the agent rewrote five files last turn, a follow-up that needs
to "see what changed" can either re-read them (paying to dump
contents into the conversation again) or reference a recorded diff.

Squeezy implements all three: checkpoint-anchored resume from
`events.jsonl`, model-curated file memory under `~/.squeezy/memory/` indexed
by `~/.squeezy/MEMORY.md`, durable note tools backed by the local store, and a
journal-backed checkpoint provider that snapshots the worktree on every edit.

## Mechanism

### On-disk layout

Sessions live under `.squeezy/sessions/<session_id>/`. The directory
is created lazily — a process that exits without producing a real
event leaves nothing on disk.

```rust
// crates/squeezy-store/src/sessions.rs:2659-2667
fn session_root(config: &AppConfig) -> PathBuf {
    if let Some(path) = &config.session_logs.log_dir {
        return resolve_workspace_path(&config.workspace_root, path);
    }
    if let Some(root) = &config.cache.root {
        return resolve_workspace_path(&config.workspace_root, root).join("sessions");
    }
    config.workspace_root.join(".squeezy").join("sessions")
}
// crates/squeezy-store/src/sessions.rs:899-901
fn session_dir(&self, session_id: &str) -> PathBuf { self.root.join(session_id) }
```

Inside each session dir, four files are load-bearing:

- `metadata.json` — `SessionMetadata`: id, cwd, status, timestamps,
  `resume_available`, parent id, cost, metrics.
- `events.jsonl` — append-only event log. Every substantive turn step
  writes a line. Source of truth.
- `resume_state.json` — materialised resume snapshot: post-compaction
  conversation, transcript, hydrated transcript. Written by
  `write_resume_state` after each compaction and on session end.
- `replay.jsonl` — pre-parsed projection of `events.jsonl` for the TUI.

The session root also owns `index.redb`, a compact metadata index keyed by
session id. It mirrors the current `metadata.json` fields needed for fast
filtering by cwd, repo, branch, status, labels, start time, and resume
availability. The JSON files remain the source of truth: if the redb index is
missing, stale, or unreadable, `SessionStore::list` falls back to scanning
session directories and rebuilds the index opportunistically.

`start_session` (`sessions.rs:590-607`) allocates the id and timestamps
but never touches disk — the handle returns in `InnerState::Pending`
and only materialises on the first substantive append. `open_session`
(`sessions.rs:609-641`) is the read-side complement: it pre-seeds
counters from `metadata.json` and the replay tape so the first event
after resume doesn't re-trigger `first_user_task` or restart the
event counter from zero.

### Resume by checkpoint anchor

The most expensive part of replay is the conversation snapshot.
`replay_resume_state` walks `events.jsonl` newest-first looking for a
`ContextCompacted` event whose `conversation` field is non-empty.
Once found, that snapshot becomes the conversation baseline and the
function forward-applies only events after it:

```rust
// crates/squeezy-store/src/sessions.rs:2158-2206
pub fn replay_resume_state(&self) -> Result<SessionResumeState> {
    // ... pending-session early return ...
    let (events, _warnings) = read_jsonl(&self.dir().join("events.jsonl"))?;
    let mut conversation: Vec<ResumeItem> = Vec::new();
    // ... transcript / hydrated / replay state ...
    for (idx, event) in events.iter().enumerate().rev() {
        if let Some(SessionEventKind::ContextCompacted {
            conversation: snapshot,
            ..
        }) = SessionEventKind::try_from_event(event)
            && !snapshot.is_empty()
        {
            conversation = snapshot;
            // Replay only events with index > idx, in chronological
            // order — events at idx or earlier are subsumed by the
            // checkpoint snapshot.
            for forward in events.iter().skip(idx + 1) {
                apply_event_to_replay(
                    forward, &mut conversation, &mut transcript,
                    &mut hydrated, &mut replay,
                );
            }
            return Ok(/* SessionResumeState built from snapshot + forward replay */);
        }
    }
    // ... linear fallback when no checkpoint event exists ...
}
```

The function name says "replay" but the loop only walks forward from
the youngest checkpoint onward. Older events stay on disk for audit
and rollout export, but they never reach the model on resume. When no
checkpoint event is found — resuming a session that never compacted —
the function falls back to a linear forward replay from an empty
conversation.

`ContextCompacted` is the event variant that carries the snapshot:

```rust
// crates/squeezy-store/src/sessions.rs:2283-2295
ContextCompacted {
    #[serde(default)] record: Value,
    #[serde(default)] summary: Option<String>,
    #[serde(default)] replacement_id: Option<String>,
    /// Pre-compaction conversation snapshot. Populated when the
    /// producer wants replay to snap to this checkpoint instead of
    /// linear-replaying older events.
    #[serde(default)] conversation: Vec<ResumeItem>,
}
```

### Replay tape

`replay.jsonl` is a pre-parsed projection that the TUI consumes.
`replay_tape` reads it in one shot with no per-event re-parsing:

```rust
// crates/squeezy-store/src/sessions.rs:1069-1078
pub fn replay_tape(&self, session_id: &str) -> Result<SessionReplayTape> {
    let (events, warnings) =
        read_replay_jsonl(&self.locate_session_dir(session_id).join("replay.jsonl"))?;
    Ok(SessionReplayTape {
        schema_version: SESSION_REPLAY_SCHEMA_VERSION,
        session_id: session_id.to_string(),
        events, warnings,
    })
}
```

The TUI redraws from the tape instead of re-running the agent's
projection; `open_session` consults it to seed `counters.replay_count`
without scanning `events.jsonl`.

### Session fork

A fork creates a child session that inherits the parent's
post-compaction state at zero LLM cost. The parent's
`resume_state.json` is read directly, attachments are copied, the
child records the parent id:

```rust
// crates/squeezy-store/src/sessions.rs:652-690
pub fn fork_session(&self, parent_session_id: &str,
                    mut metadata: SessionMetadata) -> Result<SessionHandle> {
    let parent_dir = self.session_dir(parent_session_id);
    if !parent_dir.exists() { /* not-found error */ }
    let parent_resume: SessionResumeState = read_json(&parent_dir.join("resume_state.json"))
        .or_else(|_| {
            // Fall back to a replay when the snapshot is missing or
            // corrupt — matches `Agent::resume`'s recovery path so an
            // intact event log keeps forks possible.
            let handle = self.open_session(parent_session_id.to_string());
            handle.replay_resume_state()
        })?;
    metadata.parent_id = Some(parent_session_id.to_string());
    let handle = self.start_session(metadata)?;
    handle.write_resume_state(&parent_resume)?;
    // ... copy parent_dir/attachments into the child dir ...
    handle.append_event(SessionEvent::new(
        "session_forked", None,
        Some(format!("forked from {parent_session_id}")),
        json!({ "parent_session_id": parent_session_id }),
    ))?;
    Ok(handle)
}
```

The child does not replay the parent's `events.jsonl`. Branch point
is the parent's resume snapshot, which is already compacted. Fork
pays disk I/O — one `read_json`, one `write_resume_state`, a
directory copy of attachments — and nothing more. The fallback path
that calls `replay_resume_state` when `resume_state.json` is missing
keeps the fork viable as long as `events.jsonl` is intact.

### Cross-session memory

There are two cross-session memory surfaces, both on by default:

- **File-based memory (model-curated).** The primary surface. Durable facts
  live as Markdown topic files with YAML frontmatter (`name`, `description`,
  `metadata.type`; kinds `user`, `feedback`, `project`, `reference`), pointed to
  by a `MEMORY.md` index. The kind picks the **scope** automatically: `user` +
  `feedback` are global (`~/.squeezy/memory/`), `project` + `reference` are
  per-repo (`<workspace>/.squeezy/memory/`). At session start the agent stitches
  the memory **guidance** plus both **indexes** into base instructions (each
  index truncated to `context_compaction.user_memory_max_bytes`); topic files
  are read on demand. Two writers feed it: the model-callable `memory` tool
  (`save` / `delete` / `list` / `read`) and an automatic extraction pass — a
  cheap auxiliary LLM call after a turn settles that distils durable facts into
  memory, gated by `memory_auto_extract` (default on) plus a recorded-session +
  cheap-model + new-prose threshold so it stays cheap and never fires in
  tests/eval. The whole surface is gated by `user_memory_max_bytes > 0`.
- **Durable notes.** The model-callable `notes_remember` and `notes_recall`
  tools store and retrieve decisions, conventions, preferences, dead ends,
  and notes from the persistent `redb` store. These are queried during
  compaction summaries and can be used before re-deriving old decisions.

The legacy `SessionStore::memory_path`, `remember`, and `recall` helpers still
target lowercase `~/.squeezy/memory.md` and are distinct from the model-curated
`memory` tool above. They remain useful Rust primitives:

```rust
// crates/squeezy-store/src/sessions.rs:280-282
pub fn memory_path() -> Option<PathBuf> {
    Some(fs_util::user_squeezy_dir()?.join("memory.md"))
}
```

`remember` is the canonical append primitive. Input is trimmed; empty
is a no-op; the file is forced to end with a newline before each
append so remembered lines stay row-aligned:

```rust
// crates/squeezy-store/src/sessions.rs:295-323
pub fn remember(line: &str) -> Result<usize> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    let Some(path) = Self::memory_path() else {
        return Err(SqueezyError::Agent(format!(
            "remember requires a user profile directory to be set ({})",
            fs_util::user_squeezy_dir_detail()
        )));
    };
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    let needs_leading_newline = match fs::metadata(&path) {
        Ok(meta) if meta.len() > 0 => !memory_file_ends_with_newline(&path)?,
        _ => false,
    };
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    let mut written = 0;
    if needs_leading_newline { file.write_all(b"\n")?; written += 1; }
    file.write_all(trimmed.as_bytes())?;
    file.write_all(b"\n")?;
    written += trimmed.len() + 1;
    Ok(written)
}
```

`recall` truncates the lowercase helper file at `max_bytes` on a char
boundary, appends `\n[truncated]` when the cap fires, and returns `None`
when ingestion is disabled or the file is missing:

```rust
// crates/squeezy-store/src/sessions.rs:165-185
pub fn recall(max_bytes: usize) -> Option<String> {
    if max_bytes == 0 { return None; }
    let path = Self::memory_path()?;
    let body = fs::read_to_string(&path).ok()?;
    if body.is_empty() { return None; }
    if body.len() <= max_bytes { return Some(body); }
    let mut end = max_bytes;
    while end > 0 && !body.is_char_boundary(end) { end -= 1; }
    let mut truncated = String::with_capacity(end + "\n[truncated]".len());
    truncated.push_str(&body[..end]);
    truncated.push_str("\n[truncated]");
    Some(truncated)
}
```

The prompt-ingestion cap is `context_compaction.user_memory_max_bytes`:

```rust
// crates/squeezy-core/src/lib.rs (ContextCompactionConfig)
/// Master switch for the file-based memory feature, and the cap (in
/// bytes) on how much of the `~/.squeezy/MEMORY.md` index is stitched
/// into the base instructions at session start. When `> 0` the agent
/// injects the memory guidance plus the index and advertises the
/// model-curated surface; `0` disables the whole feature.
pub user_memory_max_bytes: usize,
```

Default is `16_384` bytes (`DEFAULT_CONTEXT_USER_MEMORY_MAX_BYTES` at
`crates/squeezy-core/src/lib.rs`), so memory is on by default. Memory
ingestion runs once at session start and, being part of the frozen base
instructions, stays in the cached prompt prefix. Durable notes are separate:
`notes_remember` writes typed observations to the `redb` store and
`notes_recall` returns recent matching notes.

### Global session index

Per-project sessions live under each workspace's `.squeezy/sessions/`,
so a resume picker that shows sessions from sibling repos needs a
cross-project surface. `global_index_path` resolves it through
`xdg_global_index_path`: when `XDG_STATE_HOME` is set to an absolute
path the surface is `$XDG_STATE_HOME/squeezy/sessions/index.jsonl`;
otherwise it is the user-global Squeezy directory
(`$HOME/.squeezy/sessions/index.jsonl`, or the Windows
profile/app-data fallback). `list_global_index` additionally merges
the legacy `$HOME/.squeezy/sessions/index.jsonl` path so sessions
recorded before an XDG move stay visible:

```rust
// crates/squeezy-store/src/sessions.rs:370-372
pub fn global_index_path() -> Option<PathBuf> {
    xdg_global_index_path()
}
```

Writes are append-only and intentionally lossy — failures are
swallowed because the per-project store remains authoritative:

```rust
// crates/squeezy-store/src/sessions.rs:208-223
pub fn append_global_index_entry(entry: &GlobalSessionIndexEntry) {
    let Some(path) = Self::global_index_path() else { return; };
    if let Some(parent) = path.parent() { let _ = fs::create_dir_all(parent); }
    let Ok(mut payload) = serde_json::to_vec(entry) else { return; };
    payload.push(b'\n');
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else { return; };
    let _ = file.write_all(&payload);
}
```

Readers dedupe by `session_id` keeping the entry with the largest
`last_event_at_ms`, sort newest-first for the picker, and retain at most
`GLOBAL_INDEX_MAX_ENTRIES` entries (400). When the file exceeds 256 KiB
(`GLOBAL_INDEX_COMPACT_THRESHOLD_BYTES`), the next read rewrites it only if
dedupe or the 400-entry cap actually removed lines; an oversized but already
minimal all-distinct index is not rewritten on every startup.

```rust
// crates/squeezy-store/src/sessions.rs:232-270
pub fn list_global_index() -> Vec<GlobalSessionIndexEntry> {
    // ... resolve path, bail if missing/unreadable ...
    let mut by_id: HashMap<String, GlobalSessionIndexEntry> = HashMap::new();
    for line in text.lines() {
        if line.trim().is_empty() { continue; }
        let Ok(entry) = serde_json::from_str::<GlobalSessionIndexEntry>(line) else { continue; };
        match by_id.get(&entry.session_id) {
            Some(existing) if existing.last_event_at_ms >= entry.last_event_at_ms => continue,
            _ => { by_id.insert(entry.session_id.clone(), entry); }
        }
    }
    let mut entries: Vec<GlobalSessionIndexEntry> = by_id.into_values().collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.last_event_at_ms));
    let trimmed_to_cap = entries.len() > GLOBAL_INDEX_MAX_ENTRIES;
    entries.truncate(GLOBAL_INDEX_MAX_ENTRIES);
    if oversized && (trimmed_to_cap || raw_lines > entries.len()) {
        let mut ordered: Vec<&GlobalSessionIndexEntry> = entries.iter().collect();
        ordered.sort_by_key(|entry| entry.started_at_ms);
        let _ = rewrite_global_index(&path, &ordered);
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.started_at_ms));
    entries
}
```

The 256 KiB threshold is documented as roughly five hundred unique sessions
at ~500B per entry, but the 400-entry cap is the hard bound after
compaction. The hot path stays a single append.

### Checkpoint provider for code edits

Edit-bearing tools — `write_file`, `apply_patch`, `notebook_edit`, and
checkpoint-eligible `shell` calls — ask the registered
`CheckpointProvider` for a pre-edit snapshot and hand it back after
the mutation. The default provider is journal-backed:

```rust
// crates/squeezy-tools/src/checkpoint_provider.rs:130-158
impl CheckpointProvider for JournalCheckpointProvider {
    fn before_edit(&self) -> Result<Option<CheckpointSnapshot>> {
        let snapshot = self.store.track_tree()?;
        Ok(Some(CheckpointSnapshot::new(snapshot)))
    }

    fn after_edit(
        &self,
        before: &CheckpointSnapshot,
        context: &CheckpointEditContext,
    ) -> Result<Option<Value>> {
        let before = before.downcast_ref::<WorkspaceSnapshot>().ok_or_else(|| {
            SqueezyError::Tool(
                "checkpoint snapshot was produced by a different provider; \
                 cannot reconcile post-edit state in JournalCheckpointProvider".to_string(),
            )
        })?;
        let record = self.store.create_checkpoint(
            before, &context.tool_name, &context.call_id,
            &context.group_id, context.status, context.coverage_warnings.clone(),
        )?;
        Ok(record.as_ref().map(checkpoint_record_to_json))
    }
}
```

`track_tree` runs `git write-tree` against a shadow repo under
`.squeezy/checkpoints/git/`. The snapshot it returns is opaque — the
registry never inspects the payload, which keeps the
`CheckpointProvider` trait stable as new snapshot shapes come online.
`create_checkpoint` diffs the before/after trees and produces a
`CheckpointRecord` carrying per-file before/after sha256, summary,
and warnings. Once a record exists, later turns can reference the
diff instead of re-reading the touched files. The turn-scoped
`group_id` collapses a multi-file turn into one rollback unit.

## Worked example

Day one. A user debugs nondeterminism in a parallel kernel for four
hours. The agent reads the kernel source (a few thousand tokens),
walks three suspect call sites with `apply_patch`/`cargo test`/
`apply_patch` cycles (each edit hits `JournalCheckpointProvider`
for `track_tree` then `create_checkpoint`), and hits the post-turn
summarize trigger four times because `cargo test` output keeps
pushing the window past `summarize_at_percent`. Each compaction emits
a `ContextCompacted` event with `conversation` populated. After the
fourth, the resume snapshot is small — verbose tool outputs are
summarised; load-bearing turns remain. The user closes the laptop.

`resume_state.json` was rewritten after each compaction, so it
already holds the post-fourth-compaction snapshot.

Day two. `squeezy sessions resume <id>`. The agent calls
`open_session`, which pre-seeds counters from `metadata.json` and
the replay tape; the model loop reads `resume_state.json` directly
— one read, fully compacted state, ready. If `resume_state.json`
were missing or corrupt, `replay_resume_state` would walk
`events.jsonl` backwards, find the fourth `ContextCompacted`
snapshot, and forward-apply only events after it: the handful of
"good night" exchanges from end-of-day-one — ten lines instead of
thirty thousand tokens.

The model does not re-derive the kernel layout, the suspect call
sites, or what `cargo test` said about run 17. The user's next
message is priced against the post-compaction head, not the
day-one verbatim.

Mid-morning, the user forks to try a different approach.
`fork_session` reads `parent_dir/resume_state.json`, writes it into
the child, copies `attachments/`, and appends a `session_forked`
event. The child has the parent's compacted state on disk and a
parent id in its metadata. No LLM call has happened. The first paid
turn in the child is priced identically to the next turn the parent
would have taken.

## Edge cases and limits

The global index file is append-only on the hot path. Dedupe and
rewrite only run when it exceeds 256 KiB, and only on the next
`list_global_index` after that crossing. Until then, reads tolerate
duplicates because dedupe runs in-memory each time. Failures on
append silently no-op — the per-project store is authoritative.
`record_global_index` skips writes when `workspace_root` sits under
`std::env::temp_dir()` while the resolved index path is under the
real HOME, a pattern unique to `cargo test` runs that point session
stores at sandboxes but never redirect HOME.

The lowercase `remember` / `recall` helpers operate on a single file — no
per-project partition, no per-thread scope, no session-id key. Static prompt
ingestion prefers uppercase `MEMORY.md`, while model-callable durable notes
use `notes_remember` / `notes_recall` in the persistent store. There is still
no automatic consolidation path that rewrites static startup memory from the
notes store. `user_memory_max_bytes` truncates at a char boundary and appends
`\n[truncated]`. Default 16 KiB; zero disables ingestion.

If `resume_state.json` is missing — a partial write lost on crash —
both `fork_session` and the agent's resume path fall back to
`replay_resume_state`. As long as `events.jsonl` is intact, the
fallback finds the most recent compaction checkpoint and rebuilds
the same shape `resume_state.json` would have written. Cost is
parsing the event log once; subsequent reads use the
freshly-written snapshot.

The checkpoint store is a shadow git repo under
`.squeezy/checkpoints/`. `track_tree` runs `git add --all` with an
`:(exclude).squeezy` pathspec so it doesn't snapshot itself, plus
per-file `:(exclude)` entries for large-file fingerprints. Large
files are tracked by size and mtime instead of being copied into
the shadow tree.

## Cost intuition

A 100-turn session averaging 2k user/assistant tokens plus 8k tokens
of tool output per turn means naive resume costs 100 * 10k = 1M
input tokens before the user types anything. Checkpoint-anchored
resume loads `resume_state.json` (a post-compaction snapshot of a
few thousand tokens) plus whatever happened after the most recent
compaction. For a session whose compaction threshold has fired
even once, resume costs roughly two orders of magnitude less than
naive replay.

Fork is cleaner still. The child pays disk I/O — one read, one
write, a directory copy — and zero LLM tokens to inherit the
parent's compacted state.

Cross-session memory shifts a recurring cost off the agent. A user
who would otherwise re-explain the same five preferences pays N
times. With `~/.squeezy/MEMORY.md` or lowercase `memory.md`, the cap is 16 KiB of input
tokens per session start, amortised across however many turns the
session runs.

Together these mechanisms turn long-session resume from a
worst-case-O(turns) cost into a fixed head plus a linear tail since
the most recent compaction — the dominant cost-saving story for
users working on multi-day projects.
