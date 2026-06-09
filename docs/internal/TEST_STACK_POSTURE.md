# Test Stack Posture

Several tests in `crates/squeezy-agent` are wrapped in a custom
`run_high_stack_test` helper that builds a Tokio multi-thread runtime
with an enlarged worker thread stack instead of using the stock
`#[tokio::test]` attribute. This is the project's *deliberate* posture
for tests that exercise the deeply nested async state machines around
`TurnRuntime::run` → `execute_tool_calls` → `flush_parallel_batch`, and
contributors should reuse it rather than rediscovering the problem.

## What it is

Two near-identical helpers exist today, sized for the locals they need to
fit:

- `crates/squeezy-agent/tests/tool_loop.rs::run_high_stack_test` — 2
  worker threads, **32 MiB** thread stack. Used by the cancellation,
  parallel-read, multi-round, and turn-orchestration integration tests
  that drive the full `TurnRuntime::run` path (see `:501`, `:644`,
  `:757`, `:845`, `:940`, `:1328`, `:3980`).
- `crates/squeezy-agent/src/lib_tests.rs::run_high_stack_async_test` — 2
  worker threads, **8 MiB** thread stack. Used by the smaller unit-test
  scenarios that still nest enough state-machine frames to overflow the
  Windows debug default (`:1908`, `:2050`, `:2145`).

Both helpers run the future on a `tokio::spawn`-ed task so the test runs
on a worker thread (which uses `thread_stack_size`), not the runtime's
main thread.

## Why it is the way it is

`TurnRuntime::run` and its nested `async fn` callees compile to large
state machines because:

- The provider stream loop holds a `LlmEvent` + permission decision +
  per-call `ToolResult` accumulator in the same `async` frame.
- `execute_tool_calls` matches on each `ToolCall`, awaits permission,
  awaits the tool's `run`, and threads the result back into the
  conversation — all within one suspended state.
- `flush_parallel_batch` dispatches concurrent reads and folds results in
  order, holding both the dispatch futures and the per-call ledger live
  across awaits.
- Per-`AgentEvent::*` temporaries created at hot-path sites contribute
  ~1 KiB each (see the variant docs on `AgentEvent::ControlToolTrace`
  and `AgentEvent::Citation`).

The default OS thread stack on macOS/ARM64 debug builds (and to a lesser
extent Windows debug builds) is too small for the combined frame. The
stock `#[tokio::test]` attribute uses that default stack, so tests that
drive a realistic turn segfault locally even when CI's Linux runners
pass.

## When to use which helper

- **Integration test in `tests/tool_loop.rs`** that drives `Agent::start_turn`
  end-to-end, with real `ScriptedProvider` traffic, real tool dispatch,
  or any parallel-read batch: wrap in `run_high_stack_test`. Six
  pre-existing tests in this file already use it; one was added in
  PR #394 (`cancelled_turn_persists_partial_cost_and_metrics` at
  `:3978`).
- **Crate-private unit test in `src/lib_tests.rs`** that touches the
  agent turn loop but doesn't need the full 32 MiB ceiling: wrap in
  `run_high_stack_async_test`. The 8 MiB ceiling is enough for the
  current tests but is a softer guardrail than the integration helper.
- **Crate-private unit test** that doesn't touch the turn loop at all
  (parsers, schema validation, pure helpers): stay on `#[tokio::test]` /
  `#[test]`. Adding the wrapper there only pays the startup cost without
  buying anything.

## Tripwires

The 32 MiB / 8 MiB ceilings are *deliberately roomy* so future
contributions don't blow them. Two adjacent guardrails keep that posture
honest:

1. Any new `AgentEvent` variant added to the producer side should aim
   to keep `sizeof(AgentEvent) <= ~1 KiB`. Variants that exceed that
   should box their payload (`Box<…>`) so the enum doesn't grow.
2. New `async fn` callees on the `TurnRuntime::run` hot path should
   avoid holding large stack-only temporaries across `.await`. Move
   non-`Send` bookkeeping into a dedicated helper that returns its
   result before the next await point, or `Box::pin(…)` the future to
   move the state machine to the heap.

When either constraint is violated the symptom is a Tokio worker thread
segfault on macOS/ARM64 debug builds — usually on a parallel-read or
subagent test that *was* passing on the same branch. Add the offender
to the appropriate `run_high_stack_test` helper as a stopgap, then file
a structural follow-up to slim the frame; do not raise the stack ceiling
without that follow-up.

## Future work

The Category3.md design note (`design/squeezy.md`, `:40`) flags the
17k-line `crates/squeezy-agent/src/lib.rs` orchestration file as the
root cause of the stack-overflow surface. Splitting that file into
focused modules around `TurnRuntime::run`, `execute_tool_calls`, and
`flush_parallel_batch` is the long-term fix — at which point the
`AgentEvent::Citation` and `AgentEvent::ControlToolTrace` variants can
have their producer sides wired up (currently deferred for exactly this
reason; see their variant docs).

Until that landing, `run_high_stack_test` is the agreed posture.
