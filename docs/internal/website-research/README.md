# Website Refresh Research

This directory holds source notes for the 2026-06 website refresh. The public
site should be distilled from these notes, not copy them wholesale.

## Recommended Story

Use "local code understanding before model context" as the primary storyline.
Cost savings are the measured outcome of that method. Provider breadth,
permissions, sessions, skills, MCP, subagents, and TUI workflows are supporting
features for real coding work.

## Subject Notes

- `cost-saving-methodology.md` - implemented token-saving mechanisms and claims
  to avoid.
- `cost-saving-data.md` - public-ready benchmark data, chart specs, and caveats.
- `languages.md` - graph-backed language support and maturity notes.
- `providers.md` - provider categories, auth routes, routing, and accounting.
- `features.md` - product feature matrix and safe wording.
- `telemetry-posthog.md` - website telemetry path and dashboard readiness.
- `install-distribution.md` - install channels, packaging, and platform caveats.
- `tui-workflows.md` - TUI workflows and visual ideas.
- `architecture-performance.md` - Rust architecture and performance claims.
- `eval-validation.md` - evaluation and benchmark infrastructure.
- `trust-privacy-safety.md` - safety, privacy, permissions, and reporting.
- `storyline-options.md` - alternate site storylines and section inventory.

## Copy Guardrails

- Do not claim Squeezy is always cheaper.
- Show benchmark losses next to wins.
- Keep "compiler-perfect" and "LSP-backed" out of public copy.
- Say unsupported languages fall back to bounded search/read tools, without
  graph confidence.
- Treat provider pricing and cache behavior as registry/provider dependent.
- Frame telemetry as browser-to-worker-to-PostHog, not direct browser PostHog.
