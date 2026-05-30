# slash-compact-roundtrip

- **Scenario:** `crates/squeezy-eval/fixtures/scenarios/slash-compact-roundtrip.toml`
- **Run dir:** `target/eval/slash-compact-roundtrip-1780100526666/`
- **Session events:** `.squeezy/sessions/1780100526752-70751-1/events.jsonl`
- **Auto-findings fired:** 0 (no rule in `crates/squeezy-eval/src/findings.rs` covers this surface)

## Headlines

1. **`replay_resume_state` misuses the `ContextCompacted.conversation` field
   and silently loses the recent slice on resume.** Severity: **critical**.
   Suspect: `crates/squeezy-store/src/sessions.rs:1547`.
2. **Manual `/compact` does not broadcast `AgentEvent::ContextCompacted`** —
   the typed dispatch path (`Agent::compact_context_manual`) only writes
   to `events.jsonl` via `log_compaction_event`. The auto-compaction
   path (and mid-turn micro-compaction) sends both the session log
   event **and** an `AgentEvent::ContextCompacted` through `self.tx`,
   which is what TUI overlays, eval capture, and any external
   subscriber listen for. Severity: **major**. Suspect:
   `crates/squeezy-agent/src/lib.rs:2406`.

## Reproduction

```
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/slash-compact-roundtrip.toml --no-triage
```

Inspect the resulting run + the session's `events.jsonl`. The scenario
seeds 4 prompt turns (8 conversation items, past `compaction_recent_items =
6`), dispatches `/compact`, then runs one post-compact prompt.

## Finding 1 — replay loses turns and summary head

### What replay should produce after this run

After `/compact`, the agent's in-memory conversation is:
```
[ UserText(<extractive summary>),
  UserText("Turn 2 ..."), AssistantText("Acknowledged turn 2 ..."),
  UserText("Turn 3 ..."), AssistantText("Acknowledged turn 3 ..."),
  UserText("Turn 4 ..."), AssistantText("Acknowledged turn 4 ..."),
  UserText("Turn 5 ..."), AssistantText("post-compact ...") ]
```
which is what `resume_state.json` records correctly (confirmed by
reading the file in the run's session dir).

### What `replay_resume_state` actually produces

Walk the path:

1. `crates/squeezy-agent/src/lib.rs:2774` `log_compaction_event` writes
   the session event with payload field `"conversation": report.dropped`.
   For our run that payload's `conversation` is exactly the **older
   slice that was summarized away** — `[Turn1_user, Turn1_assistant]`.
2. `crates/squeezy-store/src/sessions.rs:1524` `replay_resume_state`
   walks events newest-to-oldest, finds the `ContextCompacted` event,
   reads its `conversation` snapshot, and treats it as the **base** to
   start replay from. The variant's docstring at
   `crates/squeezy-store/src/sessions.rs:2274` even calls the field a
   `"Pre-compaction conversation snapshot. Populated when the producer
   wants replay to snap to this checkpoint instead of linear-replaying
   older events."` — but the producer writes the dropped slice, not the
   pre-compaction (or post-compaction) full conversation.
3. The replay then forward-applies only events with `idx > compaction_idx`.
   For our run that's: `user_message turn-5` + `assistant_completed
   turn-5`. Turns 2, 3, 4 happen **before** the compaction event in
   `events.jsonl` and are skipped.

Net effect of replay after `/compact`:
```
[ UserText("Turn 1 ..."), AssistantText("Turn 1 ack ..."),
  UserText("Turn 5 ..."), AssistantText("Turn 5 ack ...") ]
```
- Summary head is **absent** (the producer never writes it into the
  event's `conversation` field).
- Turns 2, 3, 4 are **missing**.
- Turn 1 (which compaction explicitly dropped) is **resurrected**.

`resume_state.json` and `events.jsonl` therefore disagree on the
post-compact state. The on-disk JSON is correct; the replay-fallback
path is wrong. Any consumer that hits the fallback (corrupted JSON,
missing `resume_state.json`, the explicit "rebuild from events"
recovery path) silently undoes the most recent compaction and drops
several real turns alongside it.

### Suspect line

`crates/squeezy-store/src/sessions.rs:1547` —
`conversation = snapshot;` assumes the snapshot is the post-compact
conversation that should be the starting point, but the producer at
`crates/squeezy-agent/src/lib.rs:2790` (and the auto-path at
`crates/squeezy-agent/src/lib.rs:4647`) writes the **dropped** slice.

Either:
- the producer should write the full post-compact conversation (head
  summary + recent kept items) into the event payload, or
- the consumer should treat the field as the dropped older slice,
  rebuild the summary from the event's `summary` field, and prepend
  it (and not skip pre-compaction events).

The first option is simpler — it makes the event self-contained, which
matches the docstring intent.

## Finding 2 — manual `/compact` does not emit `AgentEvent::ContextCompacted`

`crates/squeezy-agent/src/lib.rs:2380` `compact_context_manual` ends
with `self.log_compaction_event(&report)` and returns. Compare to
`crates/squeezy-agent/src/lib.rs:4634` and
`crates/squeezy-agent/src/lib.rs:5683` (auto-compaction and mid-turn
micro-compaction), where the post-compaction sequence is
`log_event(...)` **followed by** `self.tx.send(AgentEvent::ContextCompacted
{ turn_id, report }).await`.

Concrete consequence in this run: `trace.jsonl` has no
`context_compacted` envelope. The `slash_command` action surfaces as
`status: "compacted"` (the eval driver inspects the
`DispatchOutcome::Compacted` return value), so the eval harness is fine
here — but any TUI overlay, telemetry consumer, MCP listener, or
`AgentEvent`-subscribed extension that wants to react to a `/compact`
will silently miss it.

### Suspect line

`crates/squeezy-agent/src/lib.rs:2406` — add the
`self.tx.send(AgentEvent::ContextCompacted { ... })` companion call
already present in the auto-path. `turn_id` for a manual compaction
can be `self.current_turn_id().unwrap_or(TurnId::INVALID)` (or `None`
if the variant allows it — check the existing schema).

## Why no auto-finding fired

- `expect_no_tool_errors`: no tool errors occurred — the post-compact
  turn used the mock provider's scripted reply.
- `expect_final_text_contains`: the scripted reply contained
  `"post-compact"` so the soft check passed.
- No rule in `findings.rs` audits the `ContextCompacted` event shape,
  nor compares `resume_state.json` against the replay path. Future
  work: a `compact_replay_divergence` rule that re-runs
  `replay_resume_state` on the session's `events.jsonl` and compares
  to `resume_state.json` would catch finding 1 automatically.

## Severity summary

- Finding 1 (replay divergence): **critical** — data loss on resume
  after any compaction (manual or auto, since both paths share the
  buggy `report.dropped` payload).
- Finding 2 (missing `AgentEvent` broadcast): **major** — silent
  observability gap; affects external subscribers but does not
  corrupt in-process state.
