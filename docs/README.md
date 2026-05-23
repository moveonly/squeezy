# Squeezy Docs

This directory is for committed documentation about implemented behavior and completed project decisions.

Keep personal notes, motivation, research from reference agents, sketches, and ideas that are not meant to describe the current implementation outside this committed docs tree.

Current completed decisions:

- Squeezy itself is implemented fully in Rust.
- Initial supported platforms are macOS and Linux.
- Initial UI is a TUI.
- Initial semantic navigation source language is Rust.
- Additional source languages are future work.
- Unsupported source languages fall back to ordinary bounded read/grep/list tools.
- Navigation tools mean semantic graph/code-understanding operations on top of tree-sitter, not grep wrappers.
- LSP and `rust-analyzer` are not part of navigation; use toolchain/compiler commands only for build, test, and explicit verification.
- Runtime graph state starts in memory; persisted graph/cache will use `redb`; Tantivy is deferred for later full-text ranking.

Foundation runtime behavior now exists: the workspace builds, the TUI starts, and the first LLM provider shapes are OpenAI Responses streaming and Anthropic Messages streaming. Graph-backed navigation is still future work.

Tool-call cost strategy is documented in [`tool-call-saving-strategy.md`](tool-call-saving-strategy.md).

Developer setup and verification commands live in the repository root `CONTRIBUTING.md`.

Platform support details live in `docs/PLATFORMS.md`.

Validation harness details live in `docs/VALIDATION_HARNESS.md`.
