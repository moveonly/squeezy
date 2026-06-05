# Squeezy Telemetry Worker

This Cloudflare Worker is the only component that knows the PostHog project
token. Squeezy clients send anonymous telemetry to `/v1/batch`; the Worker
validates the batch envelope, accepts product event names matching
`squeezy_*`, and forwards only bounded safe properties to PostHog. Accepted
property shapes are non-negative counters, booleans, token strings, and small
count maps; raw text, paths, URLs, arrays, and arbitrary nested objects are
dropped.

The Worker also accepts consented maintainer-intake traffic:

- `POST /v1/site` validates anonymous website page-view and CTA events and
  forwards them to PostHog as `squeezy_site_*` events.
- `POST /v1/feedback` validates a small redacted text report and forwards it
  to PostHog as `squeezy_feedback_submitted`.
- `POST /v1/report` stores a redacted tar archive in private R2 storage and
  forwards only report metadata to PostHog as `squeezy_report_submitted`.

The Worker source is TypeScript because Workers run HTTP handlers on the
JavaScript runtime and TypeScript keeps the edge proxy typed without adding a
Rust-to-Wasm build path for a small validation shim.

## Secrets

Set the PostHog token as a Cloudflare Worker secret:

```sh
wrangler secret put POSTHOG_PROJECT_TOKEN
```

Do not commit the token and do not ship it in the Squeezy binary. `POSTHOG_HOST`
is not secret and defaults to the EU ingestion host, `https://eu.i.posthog.com`.

`/v1/report` also requires a private R2 bucket binding named `REPORT_BUCKET`.
Set a Cloudflare lifecycle rule on the bucket so report archives expire after
30 or 90 days.

The repository intentionally commits `wrangler.example.toml`, not the production
`wrangler.toml`. Production deployment needs a private `wrangler.toml` with the
Worker name, account, route/workers.dev setting, and `REPORT_BUCKET` binding.

## Deploy

```sh
wrangler deploy
```

Deployment is not per Squeezy release or per user session. Deploy the Worker
when its source or private `wrangler.toml` changes. Set `POSTHOG_PROJECT_TOKEN`
once, then update it only when rotating the PostHog project token. The current
default product endpoint remains the `workers.dev` URL in `squeezy-core`; a
custom domain is a production hardening option, not the committed default.

## Accepted Payloads

`POST /v1/batch` accepts at most 64 KiB and 50 product events. The batch requires
`schema_version = 1`, matching `user_id` and `install_id` UUIDs, a session id,
app version, OS, arch, and an event array. Event names must match
`squeezy_[a-z0-9_]{1,96}`; timestamps must be within 30 days in the past and 5
minutes in the future. Each event may carry up to 128 safe properties:
non-negative numbers, booleans, safe token strings, trace ids, span ids, and
small count maps with up to 16 entries. Arrays, raw text, paths, URLs, and
arbitrary nested objects are dropped or rejected before PostHog forwarding.

`POST /v1/site` accepts at most 16 KiB and only
`squeezy_site_page_view`, `squeezy_site_cta_clicked`, or
`squeezy_site_outbound_clicked`. Referrer and target kinds are closed enums;
UTM, CTA, path, and target fields are bounded site-local tokens.

`POST /v1/feedback` accepts at most 32 KiB of JSON and a redacted message no
larger than 16 KiB. It requires source `cli` or `tui`, matching `user_id` and
`install_id`, message byte count consistency, and a redaction count. The redacted
message is forwarded to PostHog as the feedback event's `message` property.

`POST /v1/report` accepts a tar archive no larger than 2 MiB. Metadata is carried
in `x-squeezy-*` headers: schema version, report id, reported session id, source,
app version, OS, arch, install id, user id, client session id, archive byte
count, redaction count, and comma-separated section names. The archive is stored
under `reports/<report_id>.tar` in private R2; PostHog receives only metadata and
that R2 key.

## PostHog Dashboard

The project token is only for event ingestion. Programmatic dashboard setup uses
a PostHog personal API key with dashboard, insight, and query scopes.

```sh
export POSTHOG_PERSONAL_API_KEY=...
export POSTHOG_ENVIRONMENT_ID=185494
export POSTHOG_HOST=https://eu.posthog.com
bun run setup:posthog
```

The setup script creates usage, cost, reliability, graph, feedback, and report
metadata insights. Product insights are based on `squeezy_session_summary`;
website, feedback, and report endpoints keep their separate event names.

## Smoke Test

After deployment, send one synthetic `squeezy_session_summary` batch through
the Worker:

```sh
export TELEMETRY_ENDPOINT=https://squeezy-telemetry.esqueezy.workers.dev/v1/batch
bun run smoke:worker
```

For the website endpoint:

```sh
export SITE_ENDPOINT=https://squeezy-telemetry.esqueezy.workers.dev/v1/site
bun run smoke:site
```

Then verify recent session-summary events reached PostHog, including their full
PostHog properties payload:

```sh
export POSTHOG_PERSONAL_API_KEY=...
export POSTHOG_ENVIRONMENT_ID=185494
export POSTHOG_HOST=https://eu.posthog.com
bun run smoke:posthog
```

Recommended production controls:

- Cloudflare WAF or rate limiting for `POST /v1/site`.
- Cloudflare WAF or rate limiting for `POST /v1/batch`.
- Cloudflare WAF or rate limiting for `POST /v1/feedback` and
  `POST /v1/report`.
- R2 lifecycle expiry for private report archives.
- A custom domain such as `https://telemetry.squeezy.dev`.
- PostHog project settings with person profiles disabled unless explicitly
  needed later.

The endpoint intentionally has no client secret. A shipped client secret would
not protect a public binary. The protection boundary is that the PostHog token
never leaves the Worker, payloads are small, and unsafe product properties are
dropped before forwarding.
