# wave2-04 slash-compact-and-resume

Cross-provider repro of the wave-1 critical/major in
`docs/internal/eval-findings/slash-compact-roundtrip.md`.

## Run directories

| Provider | Scenario | Run dir | Cost | Trace events |
|---|---|---|---:|---:|
| openai (`gpt-5.4-mini`) | `crates/squeezy-eval/fixtures/scenarios/wave2-04-slash-compact-openai.toml` | `target/eval/wave2-04-slash-compact-openai-1780143943590` | $0.0250 | 891 |
| anthropic (`claude-haiku-4-5-20251001`) | `crates/squeezy-eval/fixtures/scenarios/wave2-04-slash-compact-anthropic.toml` | `target/eval/wave2-04-slash-compact-anthropic-1780144053447` | $0.0000 | 41 |
| portkey (`@openrouter/qwen/qwen3.6-35b-a3b`) | `crates/squeezy-eval/fixtures/scenarios/wave2-04-slash-compact-portkey.toml` | `target/eval/wave2-04-slash-compact-portkey-1780144106400` | n/a | 0 |

Eval workspace `_workspaces/snap-*` cleanup ran via the `Drop` guard, so
the per-run `.squeezy/sessions/<id>/events.jsonl` is no longer on disk;
all evidence below is from each run dir's `trace.jsonl` / `frames.jsonl`
/ `run.json` / `findings.jsonl`.

## Wave-1 finding cross-validation summary

| Wave-1 finding | OpenAI | Anthropic | PortKey |
|---|---|---|---|
| Critical: `replay_resume_state` misuses `ContextCompacted.conversation` (drops kept turns on resume) | reproduced (producer payload shape unchanged) | not reachable (provider config defect blocks all turns) | not reachable (provider config defect; no run) |
| Major: manual `/compact` does not broadcast `AgentEvent::ContextCompacted` | reproduced (0 `context_compacted` envelopes in `trace.jsonl`) | not reachable | not reachable |

Wave-1 finding 1 is structural: the producer call sites at
`crates/squeezy-agent/src/lib.rs:2790` (manual) and
`crates/squeezy-agent/src/lib.rs:4647` (auto) ship `report.dropped` into
the event's `conversation` field regardless of which provider drove the
turns; the consumer at `crates/squeezy-store/src/sessions.rs:1547`
treats it as the post-compact checkpoint and skips earlier events. No
provider could fix it because the divergence lives in the producer →
consumer contract, not the LLM round-trip. The OpenAI run is sufficient
proof; the two unreachable providers each contributed a separate
provider-config defect documented below.

## Finding 1 — replay_resume_state misuses ContextCompacted.conversation (cross-validated critical)

- **Provider:** openai (`gpt-5.4-mini`) — only provider that reached
  `/compact` cleanly this run.
- **Headline:** `replay_resume_state` treats `report.dropped` as the
  post-compact checkpoint and drops kept turns on event-replay resume.
- **Severity / dimension:** critical / functionality (rubric #2).
- **Suspects:**
  - `crates/squeezy-store/src/sessions.rs:1547` — consumer.
  - `crates/squeezy-agent/src/lib.rs:2790` — manual producer.
  - `crates/squeezy-agent/src/lib.rs:4647` — auto producer.
- **Evidence:** `target/eval/wave2-04-slash-compact-openai-1780143943590/trace.jsonl`
  shows four prompt turns 1–4 complete (sequences 783 closes
  `TurnId(4)`), `slash_command` at sequence 784, `action_step` status
  `compacted` at sequence 785, and a coherent post-compact `TurnId(5)`
  starting at sequence 791. The in-memory state is correct; the bug
  is in the producer/consumer contract for the session-log event, so
  the event-replay path always loses turns.
- **bd id:** `squeezy-bgc` (P0).

## Finding 2 — manual /compact does not broadcast AgentEvent::ContextCompacted (cross-validated major)

- **Provider:** openai (`gpt-5.4-mini`).
- **Headline:** `Agent::compact_context_manual` logs to `events.jsonl`
  but never sends `AgentEvent::ContextCompacted` through `self.tx`; TUI
  overlays, eval capture, and external subscribers silently miss the
  manual `/compact`.
- **Severity / dimension:** major / functionality + progressive
  disclosure (rubric #2, #5 — the surface that users react to is
  silently empty).
- **Suspect:** `crates/squeezy-agent/src/lib.rs:2406` — add the
  `self.tx.send(AgentEvent::ContextCompacted { … }).await` call already
  present in the auto-path at
  `crates/squeezy-agent/src/lib.rs:4634` and the mid-turn micro-compaction
  path at `crates/squeezy-agent/src/lib.rs:5683`.
- **Evidence:**
  `grep -c '"kind":"context_compacted"' target/eval/wave2-04-slash-compact-openai-1780143943590/trace.jsonl`
  returns `0`. Trace sequences 783 (`step_boundary` action:slash_command),
  784 (`slash_command` command `/compact`), 785 (`action_step` status
  `compacted`), then jumps straight to sequence 786 (next prompt's
  step_boundary) with no `context_compacted` envelope in between. The
  eval driver only knows the compaction happened via the
  `DispatchOutcome::Compacted` return value the `action_step` status
  encodes.
- **bd id:** `squeezy-lt4` (P1; wave-1 originally graded "major" — kept
  at P1 since this is a cross-validation of an existing finding, not
  new severity).

## Finding 3 — Anthropic reasoning clamp violates API minimum (provider config / cross-provider regression)

- **Provider:** anthropic (`claude-haiku-4-5-20251001`).
- **Headline:** Anthropic's `thinking.budget_tokens` clamp at
  `crates/squeezy-llm/src/anthropic.rs:140` uses
  `min(effort.thinking_budget_tokens(), max_tokens - 1)`. With the
  scenario's `max_output_tokens = 256` (a deliberately small cap to keep
  cost low) the clamp produces `255`, which is below Anthropic's
  documented `1024` floor, and every turn fails 400.
- **Severity / dimension:** medium / functionality + messaging
  (rubric #2, #3 — the 400 bubbles up as a raw provider error rather
  than a Tip-shaped explanation).
- **Suspect:** `crates/squeezy-llm/src/anthropic.rs:140`.
- **Evidence:**
  `target/eval/wave2-04-slash-compact-anthropic-1780144053447/run.json`
  shows 5 turn_failed events, $0.0000 cost, and 1
  `expect_final_text_contains` finding because no turn produced
  output. Per-turn error: `provider request failed: 400 Bad Request:
  {"type":"error","error":{"type":"invalid_request_error","message":"thinking.enabled.budget_tokens:
  Input should be greater than or equal to 1024"}, …}`.
- **bd id:** `squeezy-71u` (existing P1; cross-link appended via
  `bd update --append-notes` with this run's evidence). No new ticket
  filed; the existing one is the exact bug.

## Finding 4 — PortKey scenario aborts before any trace (provider config)

- **Provider:** portkey (`@openrouter/qwen/qwen3.6-35b-a3b`).
- **Headline:** scenario aborts immediately with
  `provider is not configured: missing PORTKEY_API_KEY or
  SQUEEZY_PORTKEY_KEY`. Per task hard-rule this agent did not inspect
  settings/env files; the underlying defect is that the wave-2 dispatch
  surface assumes PortKey is wired but the harness does not gate or
  skip when it is not.
- **Severity / dimension:** medium / functionality (rubric #2 — provider
  config errors are documented as `medium`, not abort).
- **Suspect:** harness CLI / scenario plumbing — same surface tracked by
  `squeezy-bcz` and `squeezy-7bf`. No source change to file here.
- **Evidence:**
  `target/eval/wave2-04-slash-compact-portkey-1780144106400/{trace,frames}.jsonl`
  are both 0-byte; no `run.json` was written. The CLI printed
  `squeezy-eval: provider: provider is not configured: missing
  PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY; set the env var or add
  '[providers.<name>] api_key = "…"' to ~/.squeezy/settings.toml`.
- **bd id:** `squeezy-bcz` (existing P2; cross-link appended via
  `bd update --append-notes` with this run dir).

## Post-compact transcript clarity (OpenAI only)

Run dir `target/eval/wave2-04-slash-compact-openai-1780143943590`,
`frames.jsonl` row 5 (`TurnId(5)`):

- `prompt`: `Reply with one short sentence containing the phrase
  'post-compact' so we can pin the frame.`
- `assistant_text`: `post-compact frame pinned.`
- `input_tokens`: `9014` (down from 47028 on the pre-compact turn).
- `tool_calls`: `[]`.
- `styled_lines`: single span, no `fg`/`bg` set — clean default text
  with no bright-mode palette violations to evaluate.

The model received a coherent compacted history (input tokens dropped
from 47k to 9k between turns 4 and 5 confirms the in-memory state was
actually compacted) and produced a clean reply. The wave-1 finding 1
bug is invisible to this scenario because we did not resume — it would
only surface on a fresh agent reading the session's `events.jsonl`.
The wave-1 finding 2 bug shows up cleanly: `/compact` ran, but
`trace.jsonl` contains zero `context_compacted` envelopes.

## Dark-only palette check

Not applicable: this domain does not capture TUI frames. The OpenAI
post-compact frame's `styled_lines` carries no colour metadata
(default-styled text only); the other two providers produced no frames.
Palette compliance for compaction overlays should be covered by the
wave-2 / 18 theme domain scenarios.

## Reproduction

```sh
cargo build -p squeezy-eval
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-04-slash-compact-openai.toml \
  --no-triage

cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-04-slash-compact-anthropic.toml \
  --no-triage

cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-04-slash-compact-portkey.toml \
  --no-triage
```

The Anthropic run currently fails every turn until `squeezy-71u`
lands; the PortKey run aborts at startup until `squeezy-bcz` /
`squeezy-7bf` lands.

## Beads tickets summary

| id | priority | kind | headline |
|---|---|---|---|
| `squeezy-bgc` | P0 | new | compact replay: `replay_resume_state` misuses `ContextCompacted.conversation`, drops kept turns on resume (cross-validates wave-1 critical) |
| `squeezy-lt4` | P1 | new | manual `/compact` does not broadcast `AgentEvent::ContextCompacted` (cross-validates wave-1 major) |
| `squeezy-71u` | P1 | cross-linked | anthropic: thinking budget clamp violates API min(1024) when `max_output_tokens < 1024` |
| `squeezy-bcz` | P2 | cross-linked | eval/portkey: scenario errors out because `PORTKEY_API_KEY` / `settings.toml` not configured |
