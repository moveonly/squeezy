# Squeezy Telemetry Worker

This Cloudflare Worker is the only component that knows the PostHog project
token. Squeezy clients send anonymous telemetry to `/v1/batch`; the Worker
validates the schema and forwards only allowlisted event names, enum values,
timestamps, and numeric counters to PostHog.

The Worker also accepts consented maintainer-intake traffic:

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

## Deploy

```sh
wrangler deploy
```

Deployment is not per Squeezy release or per user session. Deploy the Worker
when its source or `wrangler.toml` changes. Set `POSTHOG_PROJECT_TOKEN` once,
then update it only when rotating the PostHog project token.

## PostHog Dashboard

The project token is only for event ingestion. Programmatic dashboard setup uses
a PostHog personal API key with dashboard, insight, and query scopes.

```sh
export POSTHOG_PERSONAL_API_KEY=...
export POSTHOG_ENVIRONMENT_ID=185494
export POSTHOG_HOST=https://eu.posthog.com
bun run setup:posthog
```

The setup script creates usage, cost, failure, graph, feedback, and report
metadata insights on the `Squeezy Telemetry` dashboard.

## Smoke Test

After deployment, send one synthetic telemetry batch through the Worker:

```sh
export TELEMETRY_ENDPOINT=https://squeezy-telemetry.esqueezy.workers.dev/v1/batch
bun run smoke:worker
```

Then verify recent Squeezy events reached PostHog:

```sh
export POSTHOG_PERSONAL_API_KEY=...
export POSTHOG_ENVIRONMENT_ID=185494
export POSTHOG_HOST=https://eu.posthog.com
bun run smoke:posthog
```

Recommended production controls:

- Cloudflare WAF or rate limiting for `POST /v1/batch`.
- Cloudflare WAF or rate limiting for `POST /v1/feedback` and
  `POST /v1/report`.
- R2 lifecycle expiry for private report archives.
- A custom domain such as `https://telemetry.squeezy.dev`.
- PostHog project settings with person profiles disabled unless explicitly
  needed later.

The endpoint intentionally has no client secret. A shipped client secret would
not protect a public binary. The protection boundary is that the PostHog token
never leaves the Worker, payloads are small, and arbitrary strings or unknown
fields are rejected.
