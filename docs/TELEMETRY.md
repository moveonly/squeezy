# Telemetry

Squeezy sends anonymous product telemetry by default so maintainers can
understand usage, reliability, cost, and performance without collecting prompts,
file contents, paths, commands, URLs, environment variables, repository names,
or tool arguments.

Opt out with:

```sh
SQUEEZY_TELEMETRY=off
```

Accepted opt-out values are `off`, `0`, `false`, `no`, and `disabled`.

## Endpoint

The default endpoint is:

```text
https://telemetry.squeezy.dev/v1/batch
```

Override it with `SQUEEZY_TELEMETRY_ENDPOINT` when testing a local or staging
collector.

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
- `squeezy_graph_build_completed`: one-shot graph/AST build timing, file counts,
  Rust/supported/unsupported/unknown language distribution, parsed bytes,
  symbols, and edges.
- `squeezy_graph_refresh_completed`: repeated graph refresh timing, changed and
  reparsed file counts, language distribution, parsed bytes, symbols, edges, and
  refresh status.
- `squeezy_failure_seen`: coarse error kind such as provider, tool, permission,
  budget, graph, I/O, config, or unknown.

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

## PostHog Security

The Squeezy binary never contains the PostHog project token. Clients send to a
Cloudflare Worker proxy; the Worker reads `POSTHOG_PROJECT_TOKEN` from a
Cloudflare secret, validates a strict schema, rejects unknown fields, and then
forwards sanitized events to PostHog. Worker source lives in
`infra/telemetry-worker/` and is written in TypeScript.

Because the endpoint is public, abuse protection belongs at the Worker and
Cloudflare layer: request size limits, strict validation, WAF/rate limits, and
PostHog project monitoring.
