# Squeezy Site

Static Astro site for `squeezyagent.com`.

The site is intentionally static. Motion, diagrams, and interactive browser
elements run client-side, but there is no app server, database, account system,
or server-side rendering in the website itself.

The public routes live under `src/pages/`. Product facts and public copy helpers
are centralized in `src/facts.ts` and `src/config.ts`; keep claims grounded in
repo docs and implementation, not roadmap intent.

## Local Development

```sh
npm install
npm run dev
```

The local dev server defaults to:

```text
http://127.0.0.1:4321/
```

## Build

```sh
npm run build
```

Astro writes the generated site to `squeezy-site/dist/`, which is ignored.
Do not commit generated top-level `dist/` output.

Run `npm run build` before changing deploy-facing content. The app has no local
server dependency after build; Cloudflare Pages serves the static `dist/`
output.

## Cloudflare Pages

Recommended Pages settings:

```text
Framework preset: Astro
Build command: npm ci && npm run build
Build output directory: dist
Root directory: squeezy-site
Production branch: main
```

## Content Boundary

Public copy should be grounded in repo facts from `README.md`,
`crates/squeezy-skills/external-docs/`, `docs/internal/BENCHMARKS.md`, and the
implementation.
Avoid quantitative savings claims until release artifacts and auditable
benchmark traces exist.

## Website Telemetry

The site sends anonymous visitor and CTA events to the Squeezy telemetry Worker
at `/v1/site` using the endpoint in `src/config.ts`. The browser client records
`squeezy_site_page_view` on page load and `squeezy_site_cta_clicked` for links
with `data-track-click`. It respects Do Not Track, Global Privacy Control, and
the local opt-out key `squeezy_site_telemetry_opt_out=1`.

Visitor IDs live in `localStorage` under `squeezy_site_visitor_id`; per-tab
session IDs live in `sessionStorage` under `squeezy_site_session_id`. The site
privacy page documents the same behavior for users.
