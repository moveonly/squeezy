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

The same Worker also exposes consented intake endpoints for `/feedback` and
`/report`, plus a separate website visitor endpoint:

```text
https://squeezy-telemetry.esqueezy.workers.dev/v1/site
https://squeezy-telemetry.esqueezy.workers.dev/v1/feedback
https://squeezy-telemetry.esqueezy.workers.dev/v1/report
```

Those endpoints are not anonymous product telemetry from the Squeezy binary.
`/v1/site` receives anonymous website page-view and CTA events. `/feedback`
sends short redacted user text after explicit confirmation. `/report` uploads a
redacted archive to private R2 storage after explicit confirmation and forwards
only metadata to PostHog.

## Identity

Squeezy creates one random anonymous `user_id` and stores it at
`~/.squeezy/install_id`. The value is stable across sessions on that machine and
is used only to count anonymous unique users. Each process also gets a random
`session_id`. Every event has a millisecond timestamp and an increasing
`event_sequence`, so a dashboard can reconstruct the order of events within one
session.

## Events

Squeezy sends typed events with allowlisted numeric counters and enum values:

- `squeezy_app_started`: provider family and model family.
- `squeezy_turn_completed`: per-turn aggregate tool counts, read/search
  counters, output bytes, receipt stub hits, budget denials, token counters,
  cache counters, and estimated cost when available.
- `squeezy_tool_completed`: one event per first-party Squeezy tool call with
  `turn_index`, `tool_sequence`, tool name/family, status, duration in
  milliseconds, files scanned, bytes read, output bytes, and matches returned.
- `squeezy_failure_seen`: coarse error kind such as provider, tool, permission,
  budget, graph, I/O, config, or unknown.

The schema also reserves `squeezy_graph_build_completed` and
`squeezy_graph_refresh_completed` (graph build/refresh timing, file counts,
language distribution, symbols, and edges). These names are accepted by the
Worker but are not yet emitted by the binary; they are reserved for the
graph runtime that will own `GraphManager` once it is wired into the session.

Telemetry is silently disabled when the install_id cannot be loaded or
persisted (read-only `$HOME`, missing `$HOME`, ENOSPC, etc.) so that a
degraded environment does not invent a fresh anonymous user per process.

Events are buffered locally and sent in small batches, up to 50 events per
request. Squeezy flushes queued telemetry on normal CLI/TUI exit.

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
anonymous visitor/session IDs, site-local paths, coarse referrer kind, bounded
UTM fields, and CTA/target identifiers.

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
Cloudflare secret, validates a strict schema, rejects unknown fields, and then
forwards sanitized events to PostHog. Worker source lives in
`infra/telemetry-worker/` and is written in TypeScript.

Because the endpoint is public, abuse protection belongs at the Worker and
Cloudflare layer: request size limits, strict validation, WAF/rate limits, and
PostHog project monitoring.
