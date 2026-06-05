# Wave-2 bug-hunt runbook

Status: historical runbook. The initial 2026-05-30 dispatch only partially
completed, but the repository now contains all 20 wave-2 domains, with three
scenario TOMLs per domain and one finding report per domain.

Current wave-2 sources of truth:

- Plan and rubric: `docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md`.
- Scenarios: `crates/squeezy-eval/fixtures/scenarios/wave2-*.toml`.
- Finding reports: `docs/internal/eval-findings/wave2-*.md`.
- Harness behavior: `docs/internal/EVAL_HARNESS.md` and
  `crates/squeezy-eval/src/scenario.rs`.

## Current Inventory

All 20 domains have provider-specific scenarios for OpenAI, Anthropic, and
Portkey:

| # | Domain |
|--:|---|
| 01 | startup-banner-and-card |
| 02 | resume-picker-and-restore |
| 03 | slash-help-discovery |
| 04 | slash-compact-and-resume |
| 05 | slash-config-screen |
| 06 | plan-mode-question-flow |
| 07 | tool-approval-allow-deny |
| 08 | apply-patch-diff-rendering |
| 09 | mcp-elicitation-and-status |
| 10 | reasoning-toggle-and-stream |
| 11 | streaming-cancel-and-restore |
| 12 | prompt-queue-and-drain |
| 13 | tool-output-spillover |
| 14 | tool-card-coalescing |
| 15 | working-card-and-spinner |
| 16 | status-line-and-cost |
| 17 | error-and-failure-messages |
| 18 | theme-and-color-palette |
| 19 | git-and-vcs-surfaces |
| 20 | help-and-discoverability |

The finding reports are historical snapshots of the runs that produced them.
Some notes intentionally preserve defects that have since been fixed; check the
current code and issue tracker before treating a finding as still open.

## Manual Rerun Workflow

For a focused rerun:

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-<NN>-<domain>-<provider>.toml \
  --no-triage
```

Read artifacts in this order:
`run.json` -> `findings.jsonl` -> `frames_tui.jsonl` / `frames.jsonl` ->
`trace.jsonl` -> `tickets/`.

For a batch pass:

```sh
cargo run -p squeezy-eval -- check crates/squeezy-eval/fixtures/scenarios \
  --fail-on expectations,errors \
  --parallelism 4
```

Use lower parallelism for live-provider sweeps. Earlier attempts to dispatch
many model-backed workers at once hit provider rate limits and stream stalls;
the current harness can fan out scenarios, but provider quotas remain the
operator's responsibility.

## Historical Dispatch Note

The original wave-2 dispatch attempted to run many agent workers at once. That
first pass stalled under Anthropic rate limiting and stream watchdogs before
all findings were written. This file used to describe that interrupted state as
current; keep that lesson only as guidance for future live sweeps.
