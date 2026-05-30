# Squeezy Repo Guide

Squeezy is a Rust-first coding agent optimized around low cost and high-signal static code navigation.

## Engineering Principles

- Use Rust for all core systems.
- Prefer local deterministic analysis before model calls.
- Use tree-sitter parsing as the base layer for code intelligence.
- Build an incremental semantic graph over parsed files.
- Design every tool response to reduce token cost.
- Keep raw file reads as a last step, not the first step.

## Current Scope

- Implementation language: Rust only.
- Semantic navigation source languages: Rust, Python, Java, C#/.NET, Go, C/C++, JavaScript/TypeScript, and Ruby.
- Supported platforms: macOS, Linux, and Windows (x86_64). On Windows the shell sandbox is Job-Object-only and degrades filesystem and network isolation to best-effort-unavailable; `mode = "required"` denies pre-spawn.
- UI: TUI.
- Unsupported source languages fall back to ordinary bounded read/grep/list tools; do not fake graph confidence for them.
- "Navigation tools" means tree-sitter-backed semantic graph operations such as declarations, references, hierarchy, call candidates, dependency paths, impact, and exact read slices.

## Expected Architecture

The crate layout separates:

- CLI and session orchestration.
- Workspace discovery, ignore handling, and file watching.
- Tree-sitter parser runtime and language registry.
- Semantic graph storage and incremental updates.
- Session logs, context compaction, checkpoints, and resumable local work.
- Retrieval/ranking/query planning.
- Tool protocol and permission policy.
- LLM provider abstraction and prompt/cache accounting.

## Storage Direction

- Keep an in-memory query surface backed by persisted graph/cache partitions.
- Use `redb` for persisted graph/cache state.
- Hydrate graph partitions lazily when queries need them.
- Add `tantivy` later for full-text ranking; do not make it part of the first graph milestone.

## Rust Analysis Direction

- Use `tree-sitter-rust` for the hot parser/navigation path.
- Use `cargo metadata` for workspace, crate, target, feature, and dependency facts.
- Do not use LSP or `rust-analyzer` for navigation.
- Use compiler/toolchain commands for build, test, and explicit verification only.
- Keep the graph schema and navigation behavior consistent across languages.
- Attach provenance and confidence to every graph edge.

Committed implementation documentation belongs in `docs/`. Personal notes, design motivation, reference research, and uncommitted decision thinking belong outside this repository.

Skill architecture scope is documented in `docs/internal/SKILLS_SCOPE.md`.
Keep skills as local filesystem instruction bundles; do not add marketplace,
remote plugin, or extension-runtime surfaces without updating that guardrail.

Integration-test fixtures and reusable test artifacts belong under the owning crate's `tests/artifacts/` directory. If a fixture is crate-specific, keep it inside that crate (e.g. `crates/squeezy-tools/tests/artifacts/`). Do not add top-level `examples/` directories or a workspace-level `tests/artifacts/` for crate-specific fixtures.

## Test Layout

`docs/internal/TEST_LAYOUT.md` is the source of truth. Quick decision rule:

- Unit tests that need crate-private items → `src/<module>_tests.rs` paired
  with a real `src/<module>.rs` source file. Declare via `#[cfg(test)]
  #[path = "<module>_tests.rs"] mod tests;` inside `<module>.rs`. Never create
  an empty `<module>.rs` just to satisfy the pair convention.
- Integration / end-to-end tests that only use the crate's public API →
  `crates/<crate>/tests/<scenario>.rs`. Each file is its own binary and
  needs no sibling source file. Use this when the scenario is naturally a
  whole-crate exercise (e.g. host-backed smoke tests, public-API workflows).
- Cross-crate integration suites → workspace-level `tests/`.

If a new test does not have a natural source-file owner, prefer the
crate-level `tests/` directory over inventing one.

## Constraints

- Do not copy code from extracted proprietary reference implementations.
- Use open-source reference implementations only as architectural inspiration.
- Avoid adding non-Rust runtime dependencies unless explicitly approved.
- Put conventions that can be checked statically into scripts, pre-commit hooks, or CI checks instead of relying on agent instructions alone.
- Prefer durable, testable local indexes over prompt-only behavior.
- Do not run ignored `costly` integration tests from AI automation unless the user explicitly asks for a live provider check and the required Cargo feature and environment variables are already configured.

## PR Descriptions

Keep PR descriptions to a high-level functional summary. Reviewers can read the diff for the rest.

- Lead with terse bullet points describing *what* the PR does and *why*.
- Omit step-by-step implementation narration, file tours, and "how it works" walkthroughs. The diff is the source of truth for *how*.
- Only mention *how* when it is part of the functional scope — e.g. a deliberate performance trade-off, a new public API contract, a migration path, or a security-relevant choice.
- Do not include test plans, verification checklists, manual QA steps, or "I ran `cargo test`" boilerplate. CI is the verification record.
- Do not list commands run, files touched, or self-congratulatory summaries.
- No emojis, no marketing tone, no AI signatures.

If a section would just restate what the diff already shows, delete it.
