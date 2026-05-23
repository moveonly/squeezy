---
name: rust-code-navigation
description: Use Squeezy's local-first Rust code navigation workflow. Use when the user asks about Rust declarations, references, hierarchy, call candidates, dependency paths, impact, or exact read slices.
when_to_use: Rust source navigation, semantic graph inspection, and local code-understanding tasks.
triggers:
  - Rust declaration
  - Rust reference
  - call candidates
  - dependency path
  - exact read slice
---

# Rust Code Navigation

Prefer local deterministic navigation before model reasoning.

1. Start with tree-sitter-backed navigation tools when available.
2. Use `cargo metadata` facts for workspace, crate, target, feature, and dependency context.
3. Use `grep` and `glob` for broad fallback discovery before reading raw files.
4. Read bounded slices only after locating the relevant symbol or path.
5. Report provenance and confidence for semantic graph answers.

Do not claim graph confidence for unsupported languages or unresolved edges.
