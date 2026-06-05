# Telemetry

Squeezy sends anonymous product telemetry by default so maintainers can
understand usage, reliability, cost, and performance without collecting prompts,
file contents, paths, commands, URLs, environment variables, repository names,
or tool arguments.

On first CLI/TUI use, Squeezy prints a short notice with the same opt-out
command and then records that the notice was shown at
`~/.squeezy/telemetry_notice`.

Opt out with:

```sh
SQUEEZY_TELEMETRY=off
```

Accepted opt-out values are `off`, `0`, `false`, `no`, and `disabled`.

## Endpoint

The default endpoint is:

```text
https://squeezy-telemetry.esqueezy.workers.dev/v1/batch
```

Override it with `SQUEEZY_TELEMETRY_ENDPOINT` when testing a local or staging
collector.

The durable local telemetry ledger defaults to `~/.squeezy/telemetry.redb`.
Tests and staging runs can override the install id path with
`SQUEEZY_TELEMETRY_INSTALL_ID_PATH` and the ledger path with
`SQUEEZY_TELEMETRY_STORE_PATH`.

The same Worker also exposes consented intake endpoints for `/feedback` and
`/report`, plus a separate website visitor endpoint:

```text
https://squeezy-telemetry.esqueezy.workers.dev/v1/site
https://squeezy-telemetry.esqueezy.workers.dev/v1/feedback
https://squeezy-telemetry.esqueezy.workers.dev/v1/report
```

Those endpoints are not automatic product telemetry from the Squeezy binary.
`/v1/site` receives anonymous website page-view and CTA events. `/feedback`
sends short redacted user text after explicit confirmation. `/report` uploads a
redacted archive to private R2 storage after explicit confirmation when the
Worker has report storage configured, and forwards only metadata to PostHog.

## Identity

Squeezy creates one random anonymous `user_id` and stores it at
`~/.squeezy/install_id`. The value is stable across sessions on that machine and
is used only to count anonymous unique users. Each process also gets a random
`session_id`. Local telemetry facts are written with a millisecond timestamp
and an increasing local sequence, then reduced into one bounded session summary
before upload.
Local facts include correlation ids such as `trace_id`, `span_id`, and
`store_session_id` when they help connect safe runtime events in the ledger. The
remote summary remains aggregate-first and does not upload raw timelines.

## Events

Squeezy sends one typed product telemetry event per completed session:

- `squeezy_session_summary`: aggregate startup route/timing/phase durations,
  session status, turn/tool counts, graph build/refresh counters, MCP
  discovery/capability counts, external-network (websearch/webfetch) counts,
  skill activation/render counts, prompt-template expansion counts, subagent
  per-kind counts, approval/permission decision counts, provider error and
  retry counts, stop-reason and cache counts, slash/config/routing/failure
  counts, token/cost counters, and capped top-count maps for sanitized tool,
  slash-command, failure, routing, config-field, and all the new domain tokens.

The local ledger may contain detailed safe facts such as startup readiness,
turn completion, tool completion, graph build/refresh, MCP discovery,
MCP elicitation decisions, web requests, skill activation, prompt-template
expansions, approval and permission verdicts, provider errors, slash command
use, config changes, routing decisions, and coarse failures. Those local facts
are not uploaded individually by the product telemetry path.

Safe local facts are persisted in a durable telemetry ledger before network
delivery. On normal CLI/TUI exit, Squeezy stores the reduced summary as pending
before attempting upload. If upload fails or the process exits before upload
finishes, pending summaries are retried on the next startup and after explicit
`doctor` update checks. Pending summaries and their source local facts are
deleted only after the telemetry Worker returns success.

Telemetry upload is best effort and never blocks prompt readiness. If the
ledger cannot be opened, Squeezy falls back to process-local best-effort
buffering for that run. Telemetry is silently disabled when the install_id
cannot be loaded or persisted (read-only `$HOME`, missing `$HOME`, ENOSPC,
etc.) so that a degraded environment does not invent a fresh anonymous user per
process.

## What Is Never Sent

Telemetry must not include:

- user prompts or model responses,
- tool arguments,
- file contents or file snippets,
- file paths or repository names,
- shell commands or command output,
- URLs, domains from user work, or fetched web content,
- API keys, tokens, environment variable values, or settings file contents,
- exact model names when they may be user/private configured; telemetry uses
  model family buckets instead.

Website visitor telemetry is separate from product telemetry. It is limited to
pseudonymous visitor/session IDs, site-local paths, coarse referrer kind,
bounded UTM fields, and CTA/target identifiers.

These restrictions describe automatic telemetry events. Consented feedback and
report submission have their own preview, redaction, and size caps documented
in [`FEEDBACK.md`](FEEDBACK.md).

## Redaction Boundary

Telemetry remains allow-listed to coarse enums and counters. Separately, the
runtime redaction layer scrubs secret-looking text before it reaches model
requests, model-visible tool results, spilled tool-output files, provider error
events, and TUI/status surfaces. Redaction counts may appear in local status and
metrics, but raw redacted values and custom redaction patterns are not sent in
telemetry.

## PostHog Security

The Squeezy binary never contains the PostHog project token. Clients send to a
Cloudflare Worker proxy; the Worker reads `POSTHOG_PROJECT_TOKEN` from a
Cloudflare secret, validates a strict batch envelope, accepts only `squeezy_*`
product event names, and forwards bounded safe property values to PostHog.
Unsafe product properties such as raw text, paths, URLs, arrays, and arbitrary
nested objects are dropped. Worker source lives in `infra/telemetry-worker/`
and is written in TypeScript.

Because the endpoint is public, abuse protection belongs at the Worker and
Cloudflare layer: request size limits, strict validation, WAF/rate limits, and
PostHog project monitoring.
