# Squeezy Internal Docs

These docs are for contributors and maintainers. They are committed source of
truth for implementation choices, validation workflows, benchmark evidence, and
development conventions.

- [Architecture](ARCHITECTURE.md): crate boundaries and major runtime choices.
- [Semantic graph](SEMANTIC_GRAPH.md): graph schema, refresh policy,
  heuristics, traversal surface, compiler facts, and benchmark interpretation.
- [Benchmarks](BENCHMARKS.md): benchmark CLI, corpus, oracle setup, and local
  results.
- [Validation harness](VALIDATION_HARNESS.md): deterministic task harness and
  live provider smoke runners.
- [Skills scope](SKILLS_SCOPE.md): guardrails for local skill roots and
  non-goals around marketplaces, plugin runtimes, and extension APIs.
- [Test layout](TEST_LAYOUT.md): source/test file conventions enforced by
  `scripts/check_test_layout.py`.
- [Telemetry worker](TELEMETRY_WORKER.md): Cloudflare Worker deployment and
  PostHog forwarding details.

External user-help docs live in [`../external/`](../external/). Do not put
private deployment notes, implementation-only tradeoffs, benchmark details, or
contributor workflow in external docs unless a user needs that information to
operate Squeezy.
