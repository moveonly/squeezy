# Squeezy User Docs

These docs describe implemented Squeezy behavior for users. They are also the
closed local corpus embedded into built-in Squeezy help.

For the project's premise, design choices, and non-goals, see
[`../THESIS.md`](../THESIS.md).

## Use Squeezy

- [Installation](INSTALL.md): Homebrew, Cargo, GitHub release archives,
  first-run checks, upgrades, and uninstall steps.
- [Agent approach](AGENT_APPROACH.md): how Squeezy chooses local analysis,
  tools, context, and model calls.
- [Configuration](CONFIGURATION.md): settings files, environment overrides,
  permissions, budgets, TUI options, and config inspection.
- [Providers](PROVIDERS.md): supported provider adapters, model metadata, API
  key environment names, and provider status.
- [Platform support](PLATFORMS.md): macOS, Linux, release targets, and health
  checks.
- [Skills and built-in help](SKILLS.md): local `SKILL.md` directories and
  Squeezy's own `/help` surface.
- [Tools](TOOLS.md): first-party tools, slash commands, and when Squeezy uses
  each surface.

## Work With Repos

- [Semantic navigation and language coverage](LANGUAGES.md): supported source
  languages, indexed facts, fallback behavior, and known limitations.
- [Repo profile](REPO_PROFILE.md): generated local repo knowledge and suggested
  project settings.
- [Sessions](SESSIONS.md): logs, resume, export, reports, compaction, and
  retention.
- [Checkpoints](CHECKPOINTS.md): undo and revert behavior for mutating tools.

## Privacy, Cost, And External Lookup

- [Shell sandboxing](SHELL_SANDBOXING.md): permission policy, local sandboxing,
  audit records, and limits.
- [Approval policy](APPROVAL_POLICY.md): optional AI approval reviewer rules,
  allowlist boundaries, and circuit-breaker posture.
- [Telemetry](TELEMETRY.md): anonymous product telemetry and opt-out controls.
- [Feedback and reports](FEEDBACK.md): consented maintainer-intake flows.
- [Cost controls](tool-call-saving-strategy.md): receipts, budgets, caching,
  output caps, and context-saving behavior.
- [MCP and web lookup](MCP_AND_WEB.md): MCP servers, `websearch`, `webfetch`,
  and how external lookup differs from built-in help.
- [Troubleshooting](TROUBLESHOOTING.md): common startup, provider, permission,
  graph, and help issues.

## Embedded Help Boundary

Built-in Squeezy help answers only from this directory and redacted
`squeezy config inspect` output. It does not embed contributor docs from
`docs/internal/`, does not fetch the network automatically, and points users to
the public website or repository when the local corpus does not cover a topic.
