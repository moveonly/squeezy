# Sessions

Squeezy writes redacted local session history so prior work can be found and
resumed without remembering a provider response id.

By default, session state lives under `.squeezy/sessions/` in the workspace.
If `[cache].root` is set and `[session].log_dir` is unset, sessions live under
`<cache.root>/sessions`. With `[cache].root = "xdg"` on Linux, that resolves
under `$XDG_CACHE_HOME/squeezy/<repo-id>` or `$HOME/.cache/squeezy/<repo-id>`.
`[session].log_retention_days` defaults to 30 days.

Each session directory contains:

- `metadata.json`: searchable metadata, status, provider/model, repo/branch,
  cost, metrics, redaction counts, and resume availability.
- `events.jsonl`: append-only redacted user-visible events, tool calls/results,
  approvals, errors, and lifecycle events.
- `resume_state.json`: redacted model-visible conversation state used by
  `squeezy sessions resume <id>` and `/resume <id>`, including compaction
  summaries and pinned context entries.
- `attachments/`: redacted attached-context metadata plus bounded redacted text
  for pasted context and attached text files.
- `replay.jsonl`: append-only versioned redacted replay tape with model
  requests, model stream events, tool calls/results, cost decisions, timestamps,
  and stable hashes used to detect replay divergence.

`[cache].durability` controls how aggressively `events.jsonl` and
`replay.jsonl` are synced. The default `fast` mode keeps appends cheap; `turn`
syncs at explicit session flush/shutdown boundaries; `strict` syncs every
durable JSONL append.

Use CLI discovery commands:

```sh
squeezy --continue
squeezy --resume
squeezy --no-resume-picker
squeezy --session <session_id>
squeezy sessions list
squeezy sessions list --branch main --status completed --query "refactor"
squeezy sessions show <session_id>
squeezy sessions resume <session_id>
squeezy sessions fork <session_id>
squeezy sessions archive <session_id>
squeezy sessions unarchive <session_id>
squeezy sessions replay <session_id>
squeezy sessions replay <session_id> --json
squeezy sessions export <session_id>
squeezy sessions report <session_id> --preview
squeezy sessions report <session_id> --send
squeezy sessions cleanup
```

The TUI also supports `/sessions`, `/session <session_id>`, `/resume
<session_id>`, `/session rename <title>`, `/session label <label>`,
`/session-export <session_id>`, `/session-export-html <session_id>`, `/fork`,
and `/report [session_id]`.
Archiving or purging old sessions is a CLI operation (`squeezy sessions
cleanup`). `/clear` starts a fresh conversation with an empty context
window: the current conversation is finalized on disk and stays resumable via
`/resume`, while a new session takes over for subsequent turns.

`--continue` resumes the most recent resumable session for the current
directory, falling back to a fresh session when none exists. `--resume` opens
the directory-scoped resume picker, and `--session <id>` resumes an explicit
session id. When a selected session belongs to another working directory,
Squeezy prompts before resuming it from the current directory. The startup
resume picker is controlled by the config setting documented in
[`CONFIGURATION.md`](CONFIGURATION.md); `--no-resume-picker` suppresses it for
one launch.
Use `/cost` for cumulative session cost and tool accounting. It reports the
provider token counters Squeezy has received so far, cache counters when the
provider exposes them, estimated USD from local pricing metadata, tool-call
counts, subagent spend, receipt hits, spill reads/writes, redactions, and budget denials.
Provider token fields are only as complete as the provider events Squeezy has
seen; estimated USD is not a billing authority.

Use `/context` for local context accounting. It reports provider/model,
response-state mode, completed-turn counters, transcript and model-history
shape, attached-context shape, tool/result and subagent volume, and request-size estimates
for both the next transmitted request and the local full-history view. Squeezy
shows context-window percentages and remaining input budget only when it has
both model limit metadata and a deterministic local token estimate. For custom
models, unknown Ollama metadata, or other missing limits, those fields stay
`unknown`. When `store_responses=true` and a previous response id is active,
the transmitted request can be much smaller than the full local history because
the provider stores prior response state; exact provider-side current-window
usage is unknown, so `/context` labels that gap explicitly.

Attached context is managed with `/attach <path>`, `/attachments`, and
`/detach <attachment_id>`. Multi-line or large bracketed paste input is stored
as attached context; small single-line paste input stays in the prompt editor.
Long sessions compact automatically when local prompt-size estimates cross the
configured `[context]` thresholds. `/compact` forces compaction, `/pin` protects
the selected or latest transcript entry from being dropped, `/pins` lists pinned
entries, and `/unpin <pin_id>` removes a pin. Compaction events record
before/after estimated tokens in `events.jsonl`.

Session logs are local files. Prompt text, tool arguments, tool outputs,
approval metadata, provider errors, and assistant text are passed through the
shared redaction layer before persistence. Large events and sessions are
bounded by `[session].max_event_bytes` and `[session].max_session_bytes`; when a
session exceeds its byte budget it remains discoverable but is marked
non-resumable.

Replay uses the redacted tape by default. `squeezy sessions replay <id>` feeds
the recorded user turns back through the agent with a replay provider and
recorded tool results instead of live model or tool execution. Replay validates
the normalized model-request hash and tool-call hash; drift in instructions,
tool schemas, prompt shaping, or tool arguments fails the replay with a
divergence error. The replay report prints turn count, replayed event count,
request count, tool-result count, and final assistant text.

Attached context stores the original-content hash locally for dedupe, but the
stored body, preview, session events, model references, and session export use
redacted text only. The model receives compact `attachment://...` references
with metadata and a bounded redacted preview rather than full pasted/file
content. Binary files and images are recorded as unsupported and are not made
active context.

Cancelling a single turn is recorded as a `cancelled` event in `events.jsonl`
but leaves the surrounding session live so the next prompt continues to
accumulate cost, metrics, and conversation in the same session. Terminal
statuses (`failed`, `truncated`, `cancelled`) are preserved across session
finalization so a more informative outcome is never silently overwritten by the
generic `completed` status emitted on graceful exit. Resuming a session seeds
the new agent with the original session's cost, metrics, and redaction totals
so subsequent turns add to the running totals rather than replacing them.
`squeezy sessions cleanup` soft-archives live sessions by default (moved into
`archived/<id>/`, recoverable with `squeezy sessions unarchive <id>`); pass
`--purge` to hard-delete instead. The retention sweep
skips sessions that are still `running` (only explicit ids can remove a
Running session, in case it was orphaned by a crash).

Routine per-turn events (tool calls, tool results, approvals, deltas) append
to `events.jsonl` without rewriting `metadata.json`; the on-disk metadata is
only refreshed when a discovery-visible field changes (first user message,
turn-completed/failed/cancelled summary) or on resume / finish. `event_count`
remains accurate to readers via an in-memory counter, and on-disk values are
flushed at every metadata-touching boundary.
