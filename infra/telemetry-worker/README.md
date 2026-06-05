# Squeezy Telemetry Worker

Telemetry Worker documentation lives in
[`docs/internal/TELEMETRY_WORKER.md`](../../docs/internal/TELEMETRY_WORKER.md).

This directory contains the Cloudflare Worker source, deployment metadata,
PostHog dashboard setup script, and Worker tests.

Common local commands:

```sh
bun run typecheck
bun run test
bun run smoke:worker
bun run smoke:site
bun run smoke:posthog
```

`smoke:worker` requires `TELEMETRY_ENDPOINT`, `smoke:site` requires
`SITE_TELEMETRY_ENDPOINT`, and PostHog dashboard/setup checks require
`POSTHOG_PERSONAL_API_KEY`.
