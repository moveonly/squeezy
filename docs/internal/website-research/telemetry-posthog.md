# Website Telemetry And PostHog Readiness

Audit date: 2026-06-05. Scope: local repository only, no network verification.

## Short Answer

Website events do not go directly from the browser to PostHog. The static Astro
site sends page-view and tracked-click events to the Squeezy Cloudflare Worker
at `/v1/site`, and the Worker forwards accepted events to PostHog `/batch/`
using the server-side `POSTHOG_PROJECT_TOKEN`.

If the Worker is deployed with `POSTHOG_PROJECT_TOKEN` and can reach PostHog,
website events should land in PostHog as `squeezy_site_*` events. If that secret
is missing, every POST returns `telemetry_not_configured` before route handling.
If PostHog rejects ingestion, `/v1/site` returns `posthog_rejected`.

No live Cloudflare Worker, Cloudflare Pages, or PostHog project state was
verified in this audit because network access was intentionally disabled.

## Current Site Behavior

Source:

- `squeezy-site/src/config.ts`
- `squeezy-site/src/layouts/BaseLayout.astro`
- `squeezy-site/src/pages/privacy.astro`
- `squeezy-site/README.md`

The site has a hard-coded telemetry endpoint:

```ts
telemetryEndpoint: "https://squeezy-telemetry.esqueezy.workers.dev/v1/site"
```

`BaseLayout.astro` injects one client-side script on every page. It:

- exits early when `navigator.doNotTrack`, `window.doNotTrack`, Global Privacy
  Control, or `localStorage.squeezy_site_telemetry_opt_out = "1"` is present;
- stores a persistent random `visitor_id` in `localStorage`;
- stores a per-tab random `session_id` in `sessionStorage`;
- sends `squeezy_site_page_view` on load;
- sends `squeezy_site_cta_clicked` when a clicked element or ancestor has
  `data-track-click`;
- includes only `path`, coarse `referrer_kind`, bounded UTM values, `cta_id`,
  and `target_kind`;
- uses `navigator.sendBeacon` first, then a CORS `fetch` fallback with
  `credentials: "omit"` and `content-type: text/plain`.

The browser does not load PostHog JavaScript, does not contain a PostHog token,
and does not call a PostHog host directly.

The site currently emits two event names:

- `squeezy_site_page_view`
- `squeezy_site_cta_clicked`

The Worker also allow-lists `squeezy_site_outbound_clicked`, but the current site
does not emit it.

## Current Worker Behavior

Source:

- `infra/telemetry-worker/src/worker.ts`
- `infra/telemetry-worker/tests/worker.test.ts`
- `infra/telemetry-worker/wrangler.example.toml`
- local untracked `infra/telemetry-worker/wrangler.toml`

The Worker handles four routes:

- `POST /v1/batch`: product telemetry from the Squeezy binary.
- `POST /v1/site`: website page-view and CTA telemetry.
- `POST /v1/feedback`: explicit feedback text intake.
- `POST /v1/report`: explicit report archive intake, backed by R2.

For `/v1/site`, the Worker:

- allows CORS only from `https://squeezyagent.com`;
- accepts `OPTIONS` preflight;
- caps request bodies at 16 KiB;
- validates UUIDs, schema version, timestamp freshness, site-local path,
  referrer kind, event name, optional CTA/target, and optional UTM tokens;
- rejects unknown fields, including raw URLs;
- forwards one PostHog event with `distinct_id = visitor_id` and
  `$process_person_profile = false`;
- forwards through `sendPostHogBatch`, which posts to
  `${POSTHOG_HOST || "https://eu.i.posthog.com"}/batch/`.

The code does not persist website events in Cloudflare storage. The Worker is a
validation and forwarding proxy, not the durable analytics store. PostHog is the
intended analytics sink.

## Required Configuration

### Site

No runtime environment variable is currently required by the site. The telemetry
endpoint is committed in `squeezy-site/src/config.ts`.

Implications:

- Production traffic from `https://squeezyagent.com` should work with the
  current Worker CORS setting.
- Cloudflare Pages preview domains, alternate domains, and local browser tests
  will not pass Worker CORS unless the Worker allow-list changes.
- Moving to a staging Worker or custom telemetry domain requires a site source
  change today.

Recommended Pages settings are documented in `squeezy-site/README.md`:

- framework preset: Astro
- build command: `npm ci && npm run build`
- output directory: `dist`
- root directory: `squeezy-site`
- production branch: `main`

### Worker Ingestion

Required:

- `POSTHOG_PROJECT_TOKEN` as a Cloudflare Worker secret.

Optional:

- `POSTHOG_HOST`, defaulting in code and example config to
  `https://eu.i.posthog.com`.
- `REPORT_BUCKET`, only required for `POST /v1/report`.

Tracked repository state:

- `infra/telemetry-worker/wrangler.example.toml` is tracked.
- `infra/telemetry-worker/wrangler.toml` exists locally and matches the example,
  but is untracked in this workspace.

That means a clean checkout has `npm run deploy` but not the actual Wrangler
config file unless the operator copies the example or supplies config another
way. Since the current Wrangler config contains no secret, this is a deployment
readiness gap rather than a credential-safety requirement.

### PostHog Dashboard Setup

Source:

- `infra/telemetry-worker/scripts/posthog.ts`
- `infra/telemetry-worker/package.json`
- `docs/internal/TELEMETRY_WORKER.md`

Dashboard setup is automated by:

```sh
bun run setup:posthog
```

Required for dashboard setup:

- `POSTHOG_PERSONAL_API_KEY`

Optional defaults:

- `POSTHOG_ENVIRONMENT_ID=185494`
- `POSTHOG_HOST=https://eu.posthog.com`

The dashboard script creates or updates four dashboards:

- `Squeezy - 01 Product Overview`
- `Squeezy - 02 Reliability And Runtime`
- `Squeezy - 03 Website`
- `Squeezy - 04 Feedback And Reports`

Website dashboard coverage exists in code:

- `Squeezy Website Visits`: daily event counts and unique visitors for
  `squeezy_site_page_view`, `squeezy_site_cta_clicked`, and
  `squeezy_site_outbound_clicked`.
- `Squeezy Website Paths And CTAs`: path, CTA id, target kind, event count, and
  visitor count.

The script also demotes a legacy `Squeezy Telemetry` dashboard if it exists.

## Existing Verification

Local unit tests cover the Worker validation and forwarding shape:

- product telemetry forwards safe properties and drops unsafe fields;
- legacy non-`squeezy_*` product events are rejected;
- site telemetry accepts a page view and forwards sanitized PostHog properties;
- site telemetry rejects unknown fields such as `raw_url`;
- site CORS preflight returns
  `Access-Control-Allow-Origin: https://squeezyagent.com`.

Operational scripts exist:

- `bun run smoke:worker`: sends a synthetic product telemetry batch through a
  deployed Worker.
- `bun run smoke:site`: sends a synthetic site event through a deployed Worker.
- `bun run smoke:posthog`: queries recent `squeezy_session_summary` rows in
  PostHog.

## Missing Or Weak Readiness Pieces

1. `smoke:posthog` verifies product telemetry only.

   It queries recent `squeezy_session_summary` events, not `squeezy_site_*`
   events. A site smoke event can be accepted by the Worker while dashboard
   verification still provides no direct proof that site events arrived in
   PostHog.

2. Dashboard setup is scripted but not evidenced as applied.

   The repo has the dashboard definitions and setup command, but no committed
   dashboard IDs, exported snapshot, or run record proving the current PostHog
   environment has those dashboards.

3. Worker deploy config is not fully tracked.

   The tracked file is `wrangler.example.toml`; `wrangler.toml` is local and
   untracked. A clean deployment path needs either a tracked non-secret
   `wrangler.toml` or documentation that `wrangler.example.toml` must be copied
   before `npm run deploy`.

4. Site telemetry endpoint is not build-time configurable.

   This is acceptable for a single production endpoint, but it makes staging,
   local smoke tests, custom domains, and endpoint migrations require source
   edits.

5. Worker CORS is production-domain only.

   The fixed `https://squeezyagent.com` allow-list protects the endpoint from
   arbitrary browser origins, but it means Pages previews or alternate domains
   cannot send site events without a Worker change.

6. Cloudflare production controls are recommendations, not repo-managed config.

   `docs/internal/TELEMETRY_WORKER.md` recommends WAF/rate limiting, R2 lifecycle
   expiry, and a custom telemetry domain. No local IaC or Wrangler config in the
   repository proves those controls exist.

## Needed Code Changes

No required site or Worker code change is needed for the basic production path:

```text
squeezyagent.com browser
  -> https://squeezy-telemetry.esqueezy.workers.dev/v1/site
  -> https://eu.i.posthog.com/batch/
```

That path is implemented locally, assuming the Worker is deployed with
`POSTHOG_PROJECT_TOKEN`.

Recommended implementation changes:

1. Add a PostHog verification path for website events.

   Extend `infra/telemetry-worker/scripts/posthog.ts` with either
   `verify-site-posthog` or a broader `verify-posthog` query that includes
   recent `squeezy_site_*` events. Use it after `smoke:site` so the deployment
   runbook proves both Worker acceptance and PostHog arrival.

2. Track or explicitly generate Worker deploy config.

   Prefer tracking `infra/telemetry-worker/wrangler.toml` if it contains only
   non-secret Worker metadata. Otherwise, add a deploy runbook step that copies
   `wrangler.example.toml` to `wrangler.toml` before `npm run deploy`.

3. Add optional site endpoint configuration.

   Add a build-time public environment override such as
   `PUBLIC_SITE_TELEMETRY_ENDPOINT`, defaulting to the current Worker URL. This
   preserves the current production path while making staging and migration
   testable without source edits.

4. Make Worker site origins configurable if previews matter.

   Keep `https://squeezyagent.com` as the production default, but allow a
   bounded `SITE_ALLOWED_ORIGINS` list for staging and Cloudflare Pages previews.
   Keep the list explicit; do not switch to wildcard CORS.

5. Keep the Worker proxy architecture.

   Do not add PostHog browser JavaScript or a PostHog token to the static site.
   The current proxy keeps the token server-side, preserves the bounded event
   contract, and avoids introducing a broader third-party tracker surface.

## Recommended Rollout Order

1. Add `verify-site-posthog` and document the smoke order:
   `smoke:site` first, PostHog site verification second.
2. Decide whether `wrangler.toml` should be tracked or generated from the
   example, then make the deploy path reproducible from a clean checkout.
3. Add optional endpoint/origin configurability only if staging, previews, or a
   telemetry custom domain are immediate requirements.
4. Run dashboard setup with a scoped `POSTHOG_PERSONAL_API_KEY`, then record the
   resulting dashboard names or IDs in an internal runbook.
5. Confirm Cloudflare WAF/rate limits and any R2 lifecycle rule outside the repo,
   since they cannot be proven from the local source tree.
