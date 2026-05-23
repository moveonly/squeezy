# Squeezy Telemetry Worker

This Cloudflare Worker is the only component that knows the PostHog project
token. Squeezy clients send anonymous telemetry to `/v1/batch`; the Worker
validates the schema and forwards only allowlisted event names, enum values,
timestamps, and numeric counters to PostHog.

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
- A custom domain such as `https://telemetry.squeezy.dev`.
- PostHog project settings with person profiles disabled unless explicitly
  needed later.

The endpoint intentionally has no client secret. A shipped client secret would
not protect a public binary. The protection boundary is that the PostHog token
never leaves the Worker, payloads are small, and arbitrary strings or unknown
fields are rejected.
