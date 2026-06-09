# Squeezy Internal Docs

These docs are for contributors and maintainers. They are committed source of
truth for implementation choices, validation workflows, benchmark evidence, and
development conventions.

- [Architecture](ARCHITECTURE.md): workspace crate boundaries, runtime flow,
  release artifact policy, provider dependency policy, and non-goals.
- [Async and background work](ASYNC_BACKGROUND_WORK.md): job cancellation,
  session-log writer boundaries, graph watcher scope, and deferred queue/spool
  guardrails.
- [Subagents](SUBAGENTS.md): built-in subagent kinds, prompt routing,
  concurrency limits, session-log visibility, and flat-spawn invariants.
- [Checkpoints and rollback](CHECKPOINTS.md): why `RollbackTarget::Group`
  exists alongside `Latest` and `Checkpoint(id)`.
- [Semantic graph](SEMANTIC_GRAPH.md): graph schema, refresh policy,
  watcher/store behavior, heuristics, traversal surface, compiler facts, and
  benchmark interpretation.
- [Benchmarks](BENCHMARKS.md): benchmark CLI, corpus, oracle setup, and local
  results.
- [Cost-saving architecture](cost-saving/README.md): canonical detailed audit
  of prompt caching, compaction, schema loading, retrieval, routing, and
  accounting. Start with
  [unique cost features](cost-saving/00-unique-cost-features.md) for the compact
  narrative map.
- [Validation harness](VALIDATION_HARNESS.md): deterministic task harness and
  live provider smoke runners.
- [Eval harness](EVAL_HARNESS.md): scenario format, fixtures, providers,
  graders, TUI frame capture, and finding reports.
- [Skills scope](SKILLS_SCOPE.md): guardrails for local skill roots and
  non-goals around marketplaces, plugin runtimes, and extension APIs.
- [Hooks scope](HOOKS_SCOPE.md): opt-in hook events, payload boundaries, and
  non-goals.
- [Memory scope](MEMORY_SCOPE.md): persisted observation boundaries and
  deferred long-term-memory surfaces.
- [Test layout](TEST_LAYOUT.md): source/test file conventions enforced by
  `scripts/check_test_layout.py`.
- [Test stack posture](TEST_STACK_POSTURE.md): why agent turn-loop tests
  use the `run_high_stack_test` / `run_high_stack_async_test` wrappers
  instead of the stock `#[tokio::test]` attribute, and when to reuse
  them.
- [Telemetry worker](TELEMETRY_WORKER.md): Cloudflare Worker deployment and
  PostHog forwarding details.
- [Keybindings](KEYBINDINGS.md): action namespace, layered override surface
  (`settings.toml` + `~/.squeezy/keybindings.toml`), and the reserved-binding
  set.
- [Config shell escapes](CONFIG_SHELL_ESCAPES.md): why `!cmd` strings in
  `settings.toml` execute at config-load time and how that affects threat
  model for the settings file.
- [Release smoke](RELEASE_SMOKE.md): local pre-publish validation for
  every release channel (`cargo publish --dry-run`, `install.sh`,
  Homebrew formula, winget manifest) via
  `scripts/local_release_smoke.sh`.

External user-help docs live in
[`../../crates/squeezy-skills/external-docs/`](../../crates/squeezy-skills/external-docs/).
Do not put private deployment notes, implementation-only tradeoffs, benchmark
details, or contributor workflow in external docs unless a user needs that
information to operate Squeezy.
