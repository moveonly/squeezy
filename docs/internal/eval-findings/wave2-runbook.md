# Wave-2 bug-hunt runbook

Status: **partial** — scenarios shipped for 14 of 20 domains; live-run
execution + per-finding write-ups blocked by Anthropic API rate
limiting on the dispatching account.

## What happened

The wave-2 plan dispatched 20 Opus subagents in parallel, each in a
fresh worktree, each instructed to write three live scenarios (openai
gpt-5.4-mini, anthropic claude-haiku-4-5, portkey
@openrouter/qwen/qwen3.6-35b-a3b), run them, file Beads tickets for
defects, and ship a finding doc.

What actually landed:

- **Scenarios shipped** for 14 of 20 domains, three per domain (42
  TOMLs total). They sit under
  `crates/squeezy-eval/fixtures/scenarios/wave2-*.toml` and parse
  cleanly. None of them have been executed against a live provider.
- **No finding docs shipped**. Every agent stalled before reaching the
  `cargo run ...` / `bd create ...` / write-up phase. The two failure
  modes the harness reported:
  1. `API Error: Server is temporarily limiting requests (not your
     usage limit) · Rate limited` — Anthropic backed off the
     dispatching account once ~14 Opus workers tried to stream tool
     output concurrently.
  2. `Agent stalled: no progress for 600s (stream watchdog did not
     recover)` — workers that started later hit a 10-minute stream
     stall (most likely waiting on a cargo build or an outbound API
     call that the rate limiter had paused).
- **One agent (domain 01, original a90ff4e0228e09e92) was killed**
  earlier by the safety classifier when it tried to read
  `~/.squeezy/settings.toml` to verify the Portkey key was wired. A
  retry was dispatched with a "do not probe credential files"
  guardrail; the retry then ran into the same Anthropic rate-limit
  wall.

## Domains shipped (scenarios only, no findings)

| # | Domain | Scenarios |
|--:|---|---|
| 01 | startup-banner-and-card | openai / anthropic / portkey |
| 02 | resume-picker-and-restore | openai / anthropic / portkey |
| 04 | slash-compact-and-resume | openai / anthropic / portkey |
| 05 | slash-config-screen | openai / anthropic / portkey |
| 06 | plan-mode-question-flow | openai / anthropic / portkey |
| 07 | tool-approval-allow-deny | openai / anthropic / portkey |
| 08 | apply-patch-diff-rendering | openai / anthropic / portkey |
| 09 | mcp-elicitation-and-status | openai / anthropic / portkey |
| 10 | reasoning-toggle-and-stream | openai / anthropic / portkey |
| 11 | streaming-cancel-and-restore | openai / anthropic / portkey |
| 12 | prompt-queue-and-drain | openai / anthropic / portkey |
| 13 | tool-output-spillover | openai / anthropic / portkey |
| 17 | error-and-failure-messages | openai / anthropic / portkey |
| 19 | git-and-vcs-surfaces | openai / anthropic / portkey |

## Domains not attempted (no worktree output)

03 slash-help-discovery; 14 tool-card-coalescing; 15
working-card-and-spinner; 16 status-line-and-cost; 18
theme-and-color-palette; 20 help-and-discoverability.

These are still queued in `EVAL_COVERAGE_PLAN_WAVE2.md` and should be
the first batch a future wave picks up.

## How to drain the queue manually

For each shipped scenario, the next operator runs:

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-<NN>-<domain>-<provider>.toml \
  --no-triage
```

Then reads, in this order, per the harness docs:
`run.json` → `findings.jsonl` → `frames.jsonl` → `trace.jsonl` →
`tickets/`.

For each defect found:

```sh
bd create --type bug --priority P1 \
  --title "<headline>" \
  --description "<rubric dimension + evidence + scenario id + trace seq>"
```

Then write `docs/internal/eval-findings/wave2-<NN>-<domain>.md` using
the template in `docs/internal/EVAL_COVERAGE_PLAN.md`.

## Rate-limit takeaway for the next wave

Dispatching 20 Opus agents in one batch saturated Anthropic for the
dispatching account. Future waves should either:

- cap parallelism to 4–6 simultaneous agents (run in 4 waves), or
- mix Opus + Sonnet agents so per-token rate limits spread, or
- have the dispatcher pre-warm a queue, fire one agent every 30s, and
  watchdog-restart on rate-limit responses.
