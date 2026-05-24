# Squeezy Docs

This directory separates user-facing product documentation from contributor and
implementation documentation.

## External Docs

[`external/`](external/) is the public, user-help corpus. These docs explain how
Squeezy behaves, which options users can set, how the agent approaches work, and
how to diagnose common product issues. The built-in Squeezy help implementation
embeds this directory so `/help <topic>` and natural-language Squeezy questions
can answer from local docs before any model or network lookup.

Start with [`external/README.md`](external/README.md).

## Internal Docs

[`internal/`](internal/) is for contributors and maintainers. These docs cover
architecture, implementation choices, validation, benchmarks, test layout, and
deployment details. Internal docs are not embedded into normal user help.

Start with [`internal/README.md`](internal/README.md).

## Maintenance Rule

When product behavior changes, update the relevant external doc first if a user
could ask Squeezy about that behavior. When contributor workflow, architecture,
or validation behavior changes, update the relevant internal doc. If a moved or
renamed external doc should remain answerable through in-product help, update
`crates/squeezy-skills/src/help.rs` and its tests in the same change.
