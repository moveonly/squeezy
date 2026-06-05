# Eval coverage plan: scenario-driven bug hunt

Status: historical dispatch board for the 2026-05-30 bug hunt. The corresponding
scenarios, eval harness features, and finding reports have since landed; use
`EVAL_HARNESS.md` and `docs/internal/eval-findings/README.md` for current
operator guidance.

This document inventories the areas of squeezy that should each have at
least one driving scenario under `crates/squeezy-eval/fixtures/scenarios/`
and acts as the dispatch board for parallel bug-hunting agents. Each
area gets:

- a one-line **probe** of what the scenario should drive,
- a **harness shape** (mock-only / live-cheap / TUI-capture) so the
  triaging agent knows which template to copy,
- an **expected risk** column noting suspected fragility — this is
  where past changes landed, where state is shared across crates, or
  where the UI surface is most user-facing.

When a probing agent finds a reproducible defect it writes:

1. `crates/squeezy-eval/fixtures/scenarios/<area-id>.toml` — the
   scenario reproducing the bug.
2. `docs/internal/eval-findings/<area-id>.md` — the bug write-up
   (one finding per file; see template at bottom of this doc).

## Coverage matrix

| Area | Probe | Harness shape | Expected risk |
|---|---|---|---|
| `session-resume-picker` | After the recent picker → start speed-up, does Resume on a session with no events.jsonl land in a usable transcript? Does `Resuming session…` actually paint on first frame? | TUI-capture, mock provider | High — touched by #151/#153, race between deferred graph and first turn |
| `slash-compact-roundtrip` | `/compact` mid-turn, then resume from the compaction checkpoint. Does the resume_state.json carry the post-compact transcript without doubling? | live-cheap (mock) | Medium — checkpoint plumbing is silent on failure |
| `plan-mode-question-styling` | Plan-mode `request_user_input` modal — question renders in violet bold, choices not bold (post-#154); freeform Answer › field accepts text and Esc cancels without leaking the prompt | TUI-capture, mock | Low-medium — just changed in #154, regression target |
| `approval-deny-shell` | Approval popup for a `shell` call: Deny followed by Ctrl+R restores the cancelled prompt without re-firing the tool | TUI-capture, mock | High — multi-state UI, history interplay |
| `mcp-elicitation-form` | MCP elicitation Form kind: response field handles long input + Esc; the elicitation message renders in violet bold per the new style | TUI-capture, mock | Medium — rarely exercised, schema reuse |
| `cost-cap-warning` | Hit the `max_session_cost_usd_micros` cap mid-turn; status bar should switch to `cost-cap reached` and prompt queue must NOT auto-drain | mock, scripted spending | Medium — depends on cost accounting accuracy |
| `prompt-queue-drain` | Queue 3 prompts via `Shift+Enter`, run the first, cancel the second, drain proceeds to the third without dropping the cancelled one | TUI-capture, mock | High — recent change (#143/#145), state machine |
| `tool-output-spillover` | Tool produces >`spill_threshold_bytes`; preview renders, full output spills to disk, `/output` (or whatever surface) recovers it | mock with synthetic shell output | Medium — disk path silent on failure |
| `graph-unavailable-fallback` | Fire a graph tool inside the `GRAPH_READY_WAIT` window after start (post-#153); confirm we either wait gracefully or fall back to glob/grep without exploding | mock, raced startup | High — new path, never user-exercised |
| `theme-reload` | Edit `~/.squeezy/settings.json` mid-session to flip theme; first redraw should re-tone all surfaces without restart | TUI-capture, file mutation action | Low-medium — settings_watcher polling |
| `working-card-coalescing` | Five same-tool calls in a turn — they should coalesce into a single card per #145 | TUI-capture, mock with scripted tool spam | Medium — recent change |
| `bang-shell-prompt-distinct` | `!ls` prompt should render with bang-shell styling distinct from a regular prompt (#142) | TUI-capture | Low — visual regression target |

## Dispatch order

The first six areas in the matrix are dispatched in parallel as the
initial wave. The remaining six are queued for a second wave once the
first surfaces any cross-cutting infrastructure issues.

## Finding write-up template

```md
# <area-id>: <one-line headline>

## Severity

<low | medium | high>  — <why this severity, in one sentence>

## What you should see vs. what you see

- Expected: <one bullet>
- Observed: <one bullet>

## Reproducer

`cargo run -p squeezy-eval -- run crates/squeezy-eval/fixtures/scenarios/<area-id>.toml`

Optional manual repro:

1. …
2. …

## Evidence

- `frames.jsonl` line N: `<excerpt>`
- `trace.jsonl` seq M: `<excerpt>`
- `findings.jsonl` rule_id `<...>`

## Suspected cause

<short hypothesis pointing at file:line>
```
