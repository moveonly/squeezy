# Sessions

Squeezy writes redacted local session history so prior work can be found and
resumed without remembering a provider response id.

By default, session state lives under `.squeezy/sessions/` in the workspace.
If `[cache].root` is set and `[session].log_dir` is unset, sessions live under
`<cache.root>/sessions`. `[session].log_retention_days` defaults to 30 days.

Each session directory contains:

- `metadata.json`: searchable metadata, status, provider/model, repo/branch,
  cost, metrics, redaction counts, and resume availability.
- `events.jsonl`: append-only redacted user-visible events, tool calls/results,
  approvals, errors, and lifecycle events.
- `resume_state.json`: redacted model-visible conversation state used by
  `squeezy sessions resume <id>` and `/resume <id>`.
- `attachments/`: redacted attached-context metadata plus bounded redacted text
  for pasted context and attached text files.

Use CLI discovery commands:

```sh
squeezy sessions list
squeezy sessions list --branch main --status completed --query "refactor"
squeezy sessions show <session_id>
squeezy sessions resume <session_id>
squeezy sessions export <session_id>
squeezy sessions report <session_id> --preview
squeezy sessions report <session_id> --send
squeezy sessions cleanup
```

The TUI also supports `/sessions`, `/session <session_id>`, `/resume
<session_id>`, `/session-export <session_id>`, `/report [session_id]`, and
`/session-cleanup`.
Use `/cost` for cumulative session cost and tool accounting. It reports the
provider token counters Squeezy has received so far, cache counters when the
provider exposes them, estimated USD from local pricing metadata, tool-call
counts, receipt hits, spill reads/writes, redactions, and budget denials.
Provider token fields are only as complete as the provider events Squeezy has
seen; estimated USD is not a billing authority.

Use `/context` for local context accounting. It reports provider/model,
response-state mode, completed-turn counters, transcript and model-history
shape, attached-context shape, tool/result volume, and request-size estimates
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

Session logs are local files. Prompt text, tool arguments, tool outputs,
approval metadata, provider errors, and assistant text are passed through the
shared redaction layer before persistence. Large events and sessions are
bounded by `[session].max_event_bytes` and `[session].max_session_bytes`; when a
session exceeds its byte budget it remains discoverable but is marked
non-resumable.

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
`squeezy sessions cleanup` and the TUI `/session-cleanup` command refuse to
delete the currently active session, and the retention sweep skips sessions
that are still `running` (only explicit ids can remove a Running session, in
case it was orphaned by a crash).

Routine per-turn events (tool calls, tool results, approvals, deltas) append
to `events.jsonl` without rewriting `metadata.json`; the on-disk metadata is
only refreshed when a discovery-visible field changes (first user message,
turn-completed/failed/cancelled summary) or on resume / finish. `event_count`
remains accurate to readers via an in-memory counter, and on-disk values are
flushed at every metadata-touching boundary.
