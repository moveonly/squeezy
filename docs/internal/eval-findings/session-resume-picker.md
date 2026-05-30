# session-resume-picker — eval finding

## Area

`session-resume-picker` (assigned slice from `EVAL_COVERAGE_PLAN.md`).

## Scenario

- File: `crates/squeezy-eval/fixtures/scenarios/session-resume-picker.toml`
- Mode: offline, `provider = "mock"`, `[tui_capture] enabled = true,
  drive_tui = true` (live `TuiHarness` + headless `TestBackend`).
- Two prompt turns ("ping" → "pong", "again" → "pong-2"); two
  `tui_frame_contains` assertions pin that the rendered frame carries
  each reply.

## Run artifacts

- Clean run: `target/eval/session-resume-picker-1780100557399/`
  - `trace.jsonl`: 8 events, every `assert` status =
    `asserted_pass` (lines 4 and 8).
  - `findings.jsonl`: empty.
  - `tickets/`: empty.
  - `run.json`: 0 findings, 0 cost, 0 tool errors.

## Probe summary

The probe spec asked three questions:

1. **Does Resume on a near-zero-event session land in a usable
   transcript?** The harness cannot exercise `Agent::resume` directly
   (see "Harness coverage gap" below), but the closest reachable
   analog — `Agent::new` followed by a single short scripted turn —
   produces a usable transcript: the frame paints the reply, the
   second prompt fires, and the second reply also paints. No race
   surfaced between the prompt landing and `GraphManager::open_with_store`
   completing.

2. **Does `Resuming session…` paint on the first frame even on
   inline-viewport hosts?** Read of
   `crates/squeezy-tui/src/lib.rs:11781` (`draw_startup_placeholder`)
   shows it calls `terminal.draw(...)` with explicit coordinates
   `Rect { y: area.y + area.height / 2, height: 1, ... }`. The inline
   backend uses `Viewport::Inline(INLINE_VIEWPORT_HEIGHT)` where
   `INLINE_VIEWPORT_HEIGHT = 18` (line 136), so `area.height` is 18 and
   the placeholder paints at row 9. No code path skips the placeholder
   when `mode == TerminalMode::Inline`. The early-return guard
   (`if area.width == 0 || area.height == 0 { return }`) only fires in
   degenerate dimensions, not on the inline default. **Verdict: the
   placeholder does paint in inline mode.** The harness can't observe
   the placeholder frame directly (it routes through `render(...)` on
   the `TestBackend`, not `draw_startup_placeholder`) so this is a
   read-only conclusion, not a runtime assertion.

3. **Race between the TUI accepting a prompt and `Agent::resume`
   restoring state, given deferred `GraphManager::open_with_store`?**
   Inspection of `run_inner` (`crates/squeezy-tui/src/lib.rs:437-526`)
   shows `Agent::resume` runs synchronously on the main task before
   `TuiApp::new` is constructed. The deferred work backgrounded after
   the picker exits is `proposed_plan` housekeeping
   (`tokio::task::spawn_blocking` at line 486), not graph open —
   `GraphManager::open_with_store` runs inside `Agent::build`, which
   is part of the synchronous `Agent::resume` call. The TUI cannot
   accept input until after `Agent::resume` returns and `app.turn_rx`
   is wired. **Verdict: no race.** A regression where the graph open
   moved to a background task would surface as a tool error or stuck
   turn on the second prompt; the scenario's second-turn
   `tui_frame_contains "pong-2"` assertion is the regression guard
   for that.

## Severity

**No defect found — recording as a regression guard.**

The scenario stays in the fixtures directory as the canonical
`session-resume-picker` probe. When the harness grows the missing
coverage (see below) the scenario should be extended; until then it
pins the closest-reachable behaviour and will fail loud if either
turn stops rendering or if a background-graph regression makes the
second turn stall.

## Harness coverage gap (separate from the probe verdict)

`TuiHarness::new` (`crates/squeezy-tui/src/testing.rs:43-69`) only
calls `Agent::new`; there is no `TuiHarness::new_resume` /
`TuiHarness::with_existing_session` constructor. As a result, no
eval scenario today can:

- exercise the `maybe_pick_resume_session` overlay,
- exercise the `Agent::resume` → `from_resume` path,
- observe whether `draw_startup_placeholder` actually paints
  "Resuming session…" on a real first frame.

This is not a production bug; it's a coverage gap in
`squeezy-eval`. Fixing it would be a single ~50-line addition to
`testing.rs` plus a `[squeezy] resume_session_id = "..."` overlay
in `scenario.rs`. Tracking suggestion: open a follow-up ticket
("eval: TuiHarness needs a resume entry point") rather than rolling
it into this finding, because the production code being probed is
healthy.

## Repro

```sh
cargo run -p squeezy-eval -- \
  run crates/squeezy-eval/fixtures/scenarios/session-resume-picker.toml \
  --no-triage
```

Expected: `findings: 0`, both `tui_frame_contains` assertions
`asserted_pass`. Any other outcome is a regression.

## Evidence

- `target/eval/session-resume-picker-1780100557399/trace.jsonl` line
  4: `"check":{"kind":"tui_frame_contains","text":"pong"},
  "status":"asserted_pass"`.
- Same file, line 8:
  `"check":{"kind":"tui_frame_contains","text":"pong-2"},
  "status":"asserted_pass"`.
- `crates/squeezy-tui/src/lib.rs:466` — placeholder is dispatched
  unconditionally before `Agent::resume`/`Agent::new` on the same
  task.
- `crates/squeezy-tui/src/lib.rs:11781-11814` —
  `draw_startup_placeholder` has no inline-viewport short-circuit.
- `crates/squeezy-tui/src/testing.rs:51` — `TuiHarness::new` hard-codes
  `Agent::new`; documents the coverage gap.
