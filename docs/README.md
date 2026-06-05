# Squeezy Docs

This directory contains contributor and implementation documentation. The
user-facing product help corpus now lives in the `squeezy-skills` crate so Cargo
can package the same files that built-in `/help` embeds.

## User Help Corpus

[`../crates/squeezy-skills/external-docs/`](../crates/squeezy-skills/external-docs/)
is the public, user-help corpus. These docs explain how Squeezy behaves, which
options users can set, how the agent approaches work, and how to diagnose common
product issues. The `squeezy-skills` build script embeds that directory so
`/help <topic>` and natural-language Squeezy questions can answer from local
docs before any model or network lookup.

Start with
[`external-docs/README.md`](../crates/squeezy-skills/external-docs/README.md).

## Internal Docs

[`internal/`](internal/) is for contributors and maintainers. These docs cover
architecture, implementation choices, validation, benchmarks, test layout, and
deployment details. Internal docs are not embedded into normal user help.

Start with [`internal/README.md`](internal/README.md).

## Maintenance Rule

When product behavior changes, update the relevant
`crates/squeezy-skills/external-docs/` file first if a user could ask Squeezy
about that behavior. When contributor workflow, architecture, or validation
behavior changes, update the relevant internal doc. If a moved or renamed
external doc should remain answerable through in-product help, update
`crates/squeezy-skills/src/help.rs` and its tests in the same change.
