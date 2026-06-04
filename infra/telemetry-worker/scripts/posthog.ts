const DEFAULT_POSTHOG_HOST = "https://eu.posthog.com";
const DEFAULT_ENVIRONMENT_ID = "185494";
const LEGACY_DASHBOARD_NAME = "Squeezy Telemetry";

const DASHBOARDS = {
  product: {
    name: "Squeezy - 01 Product Overview",
    description: "Usage, versions, platforms, event freshness, and cost.",
  },
  reliability: {
    name: "Squeezy - 02 Reliability And Runtime",
    description: "Failures, tool behavior, graph performance, routing, sandbox, and reviewer counters.",
  },
  website: {
    name: "Squeezy - 03 Website",
    description: "Anonymous website page views, CTA clicks, referrers, and paths.",
  },
  intake: {
    name: "Squeezy - 04 Feedback And Reports",
    description: "Explicit feedback and report intake metadata.",
  },
} as const;

type DashboardKey = keyof typeof DASHBOARDS;

type JsonObject = Record<string, unknown>;

type InsightSpec = {
  dashboard: DashboardKey;
  name: string;
  description: string;
  query: string;
};

const INSIGHTS: InsightSpec[] = [
  {
    dashboard: "product",
    name: "Squeezy Ingestion Freshness",
    description: "Latest event timestamp by event name. Use this first when the dashboard looks empty.",
    query: `
SELECT
  event,
  count() AS events,
  uniq(distinct_id) AS distinct_ids,
  max(timestamp) AS latest
FROM events
WHERE startsWith(event, 'squeezy_')
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY event
ORDER BY latest DESC
LIMIT 100
`.trim(),
  },
  {
    dashboard: "product",
    name: "Squeezy Event Volume",
    description: "Daily event volume by Squeezy event name.",
    query: `
SELECT
  toDate(timestamp) AS day,
  event,
  count() AS events
FROM events
WHERE startsWith(event, 'squeezy_')
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY day, event
ORDER BY day ASC, event ASC
`.trim(),
  },
  {
    dashboard: "product",
    name: "Squeezy Recent Raw Events",
    description: "Recent Squeezy telemetry rows with the full PostHog properties payload for ingestion debugging.",
    query: `
SELECT
  timestamp,
  event,
  distinct_id,
  properties
FROM events
WHERE startsWith(event, 'squeezy_')
  AND timestamp > now() - INTERVAL 24 HOUR
ORDER BY timestamp DESC
LIMIT 200
`.trim(),
  },
  {
    dashboard: "product",
    name: "Squeezy DAU",
    description: "Daily anonymous unique users from completed session summaries.",
    query: `
SELECT
  toDate(timestamp) AS day,
  uniq(properties.user_id) AS users,
  count() AS sessions
FROM events
WHERE event = 'squeezy_session_summary'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY day
ORDER BY day ASC
`.trim(),
  },
  {
    dashboard: "product",
    name: "Squeezy Version Distribution",
    description: "Recent app versions seen in telemetry.",
    query: `
SELECT
  properties.app_version AS app_version,
  count() AS events,
  uniq(properties.user_id) AS users
FROM events
WHERE event = 'squeezy_session_summary'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY app_version
ORDER BY events DESC
LIMIT 20
`.trim(),
  },
  {
    dashboard: "website",
    name: "Squeezy Website Visits",
    description: "Anonymous website page views and CTA clicks.",
    query: `
SELECT
  toDate(timestamp) AS day,
  event,
  count() AS events,
  uniq(properties.visitor_id) AS visitors
FROM events
WHERE event IN ('squeezy_site_page_view', 'squeezy_site_cta_clicked', 'squeezy_site_outbound_clicked')
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY day, event
ORDER BY day ASC, event ASC
`.trim(),
  },
  {
    dashboard: "website",
    name: "Squeezy Website Paths And CTAs",
    description: "Website paths and clicked calls to action.",
    query: `
SELECT
  properties.path AS path,
  properties.cta_id AS cta_id,
  properties.target_kind AS target_kind,
  count() AS events,
  uniq(properties.visitor_id) AS visitors
FROM events
WHERE event IN ('squeezy_site_page_view', 'squeezy_site_cta_clicked', 'squeezy_site_outbound_clicked')
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY path, cta_id, target_kind
ORDER BY events DESC
LIMIT 50
`.trim(),
  },
  {
    dashboard: "product",
    name: "Squeezy OS And Arch",
    description: "Anonymous platform distribution.",
    query: `
SELECT
  concat(toString(properties.os), ' / ', toString(properties.arch)) AS platform,
  count() AS events,
  uniq(properties.user_id) AS users
FROM events
WHERE event = 'squeezy_session_summary'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY platform
ORDER BY users DESC
`.trim(),
  },
  {
    dashboard: "reliability",
    name: "Squeezy Tool Outcomes",
    description: "First-party tool-call outcome counters from session summaries.",
    query: `
SELECT
  toDate(timestamp) AS day,
  sum(toUInt64OrZero(toString(properties.tool_calls))) AS tool_calls,
  sum(toUInt64OrZero(toString(properties.tool_successes))) AS successes,
  sum(toUInt64OrZero(toString(properties.tool_errors))) AS errors,
  sum(toUInt64OrZero(toString(properties.tool_denials))) AS denials,
  sum(toUInt64OrZero(toString(properties.tool_cancellations))) AS cancellations
FROM events
WHERE event = 'squeezy_session_summary'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY day
ORDER BY day ASC
`.trim(),
  },
  {
    dashboard: "product",
    name: "Squeezy Session Cost And Tokens",
    description: "Session-level token and estimated cost counters.",
    query: `
SELECT
  toDate(timestamp) AS day,
  sum(toUInt64OrZero(toString(properties.input_tokens))) AS input_tokens,
  sum(toUInt64OrZero(toString(properties.output_tokens))) AS output_tokens,
  sum(toUInt64OrZero(toString(properties.cached_tokens))) AS cached_tokens,
  sum(toUInt64OrZero(toString(properties.estimated_usd_micros))) / 1000000 AS estimated_usd
FROM events
WHERE event = 'squeezy_session_summary'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY day
ORDER BY day ASC
`.trim(),
  },
  {
    dashboard: "reliability",
    name: "Squeezy Failures",
    description: "Coarse failure and subagent counters from session summaries.",
    query: `
SELECT
  toDate(timestamp) AS day,
  sum(toUInt64OrZero(toString(properties.failure_count))) AS failures,
  sum(toUInt64OrZero(toString(properties.subagent_failures))) AS subagent_failures,
  sum(toUInt64OrZero(toString(properties.budget_denials))) AS budget_denials
FROM events
WHERE event = 'squeezy_session_summary'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY day
ORDER BY day ASC
`.trim(),
  },
  {
    dashboard: "intake",
    name: "Squeezy Feedback Intake",
    description: "Explicit user-submitted feedback counts and redaction volume.",
    query: `
SELECT
  toDate(timestamp) AS day,
  properties.source AS source,
  count() AS feedback,
  sum(toUInt64OrZero(toString(properties.message_bytes))) AS message_bytes,
  sum(toUInt64OrZero(toString(properties.redactions))) AS redactions
FROM events
WHERE event = 'squeezy_feedback_submitted'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY day, source
ORDER BY day ASC, source ASC
`.trim(),
  },
  {
    dashboard: "intake",
    name: "Squeezy Report Uploads",
    description: "Explicit bug-report archive uploads with metadata only.",
    query: `
SELECT
  toDate(timestamp) AS day,
  properties.source AS source,
  count() AS reports,
  sum(toUInt64OrZero(toString(properties.archive_bytes))) AS archive_bytes,
  sum(toUInt64OrZero(toString(properties.redactions))) AS redactions
FROM events
WHERE event = 'squeezy_report_submitted'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY day, source
ORDER BY day ASC, source ASC
`.trim(),
  },
  {
    dashboard: "reliability",
    name: "Squeezy Graph Build Performance",
    description: "AST/graph build and refresh counters from session summaries.",
    query: `
SELECT
  toDate(timestamp) AS day,
  sum(toUInt64OrZero(toString(properties.graph_build_count))) AS graph_builds,
  sum(toUInt64OrZero(toString(properties.graph_refresh_count))) AS graph_refreshes,
  sum(toUInt64OrZero(toString(properties.supported_files))) AS supported_files,
  sum(toUInt64OrZero(toString(properties.unsupported_files))) AS unsupported_files,
  sum(toUInt64OrZero(toString(properties.symbols))) AS symbols,
  sum(toUInt64OrZero(toString(properties.edges))) AS edges
FROM events
WHERE event = 'squeezy_session_summary'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY day
ORDER BY day ASC
`.trim(),
  },
  {
    dashboard: "reliability",
    name: "Squeezy Routing Decisions",
    description: "Cheap-route and escalation counters from session summaries.",
    query: `
SELECT
  toDate(timestamp) AS day,
  sum(toUInt64OrZero(toString(properties.routing_routed_count))) AS routed,
  sum(toUInt64OrZero(toString(properties.routing_escalated_count))) AS escalated,
  uniq(properties.user_id) AS users
FROM events
WHERE event = 'squeezy_session_summary'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY day
ORDER BY day ASC
`.trim(),
  },
  {
    dashboard: "reliability",
    name: "Squeezy Session Status",
    description: "Completed, failed, truncated, and abnormal session status counters.",
    query: `
SELECT
  properties.session_status AS status,
  properties.abnormal_exit AS abnormal_exit,
  count() AS sessions,
  uniq(properties.user_id) AS users
FROM events
WHERE event = 'squeezy_session_summary'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY status, abnormal_exit
ORDER BY sessions DESC
`.trim(),
  },
  {
    dashboard: "reliability",
    name: "Squeezy Startup Routes",
    description: "Startup route distribution from session summaries.",
    query: `
SELECT
  properties.startup_route AS startup_route,
  count() AS sessions,
  uniq(properties.user_id) AS users
FROM events
WHERE event = 'squeezy_session_summary'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY startup_route
ORDER BY sessions DESC
LIMIT 50
`.trim(),
  },
];

async function main(): Promise<void> {
  const command = process.argv[2];
  if (command === "setup-dashboard") {
    await setupDashboard();
  } else if (command === "smoke-worker") {
    await smokeWorker();
  } else if (command === "smoke-site") {
    await smokeSite();
  } else if (command === "verify-posthog") {
    await verifyPosthog();
  } else {
    printUsage();
    process.exit(command ? 1 : 0);
  }
}

function printUsage(): void {
  console.log(`Usage:
  bun scripts/posthog.ts setup-dashboard
  bun scripts/posthog.ts smoke-worker
  bun scripts/posthog.ts smoke-site
  bun scripts/posthog.ts verify-posthog

Environment:
  POSTHOG_PERSONAL_API_KEY  Required for setup-dashboard and verify-posthog.
  POSTHOG_ENVIRONMENT_ID    Defaults to ${DEFAULT_ENVIRONMENT_ID}.
  POSTHOG_HOST              Defaults to ${DEFAULT_POSTHOG_HOST}.
  TELEMETRY_ENDPOINT        Required for smoke-worker, e.g. https://squeezy-telemetry.esqueezy.workers.dev/v1/batch.
  SITE_TELEMETRY_ENDPOINT   Required for smoke-site, e.g. https://squeezy-telemetry.esqueezy.workers.dev/v1/site.
`);
}

async function setupDashboard(): Promise<void> {
  const client = posthogClient();
  const dashboards = await ensureDashboards(client);
  await demoteLegacyDashboard(client);
  for (const dashboard of Object.values(dashboards)) {
    console.log(`Dashboard: ${dashboard.name} (${dashboard.id})`);
  }

  const created = await Promise.all(
    INSIGHTS.map((insight) => ensureInsight(client, dashboards[insight.dashboard].id, insight)),
  );
  for (const insight of created) {
    console.log(`Insight: ${insight.name} (${insight.id})`);
  }
}

async function smokeWorker(): Promise<void> {
  const endpoint = requiredEnv("TELEMETRY_ENDPOINT");
  const installId = crypto.randomUUID();
  const sessionId = crypto.randomUUID();
  const now = Date.now();
  const body = {
    schema_version: 1,
    user_id: installId,
    install_id: installId,
    session_id: sessionId,
    app_version: "smoke",
    os: "darwin",
    arch: "arm64",
    events: [
      {
        event: "squeezy_session_summary",
        timestamp_ms: now,
        event_sequence: 1,
        properties: {
          summary_id: crypto.randomUUID(),
          trace_id: "0".repeat(32),
          started_at_ms: now - 2500,
          ended_at_ms: now,
          source_records: 9,
          dropped_buckets: 0,
          abnormal_exit: false,
          telemetry_truncated: false,
          session_status: "completed",
          startup_route: "fresh",
          duration_ms: 2500,
          turn_count: 1,
          tool_calls: 1,
          tool_successes: 1,
          tool_errors: 0,
          tool_denials: 0,
          tool_cancellations: 0,
          graph_build_count: 1,
          graph_refresh_count: 0,
          slash_command_count: 1,
          config_change_count: 0,
          failure_count: 1,
          routing_routed_count: 1,
          routing_escalated_count: 0,
          subagent_calls: 0,
          subagent_failures: 0,
          provider: "open_ai",
          model_family: "gpt",
          files_scanned: 3,
          files_parsed: 3,
          supported_files: 3,
          unsupported_files: 0,
          symbols: 12,
          edges: 20,
          input_tokens: 100,
          output_tokens: 20,
          cached_tokens: 10,
          estimated_usd_micros: 1,
          receipt_stub_hits: 0,
          negative_receipt_hits: 0,
          budget_denials: 0,
          bytes_read: 128,
          output_bytes: 64,
          matches_returned: 2,
          tool_counts: { grep: 1 },
          slash_counts: { plan: 1 },
          failure_counts: { unknown: 1 },
          routing_counts: { "routed:smoke": 1 },
          config_counts: { "model.model": 1 },
        },
      },
    ],
  };

  const response = await fetch(endpoint, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  if (response.status !== 204) {
    throw new Error(`smoke-worker failed: ${response.status} ${await response.text()}`);
  }
  console.log(`Smoke batch accepted by ${endpoint}`);
}

async function smokeSite(): Promise<void> {
  const endpoint = requiredEnv("SITE_TELEMETRY_ENDPOINT");
  const body = {
    schema_version: 1,
    visitor_id: crypto.randomUUID(),
    session_id: crypto.randomUUID(),
    timestamp_ms: Date.now(),
    event: "squeezy_site_page_view",
    path: "/",
    referrer_kind: "none",
    utm_source: "smoke",
    utm_medium: "script",
    utm_campaign: "telemetry",
  };

  const response = await fetch(endpoint, {
    method: "POST",
    headers: { "content-type": "text/plain;charset=UTF-8" },
    body: JSON.stringify(body),
  });
  if (response.status !== 204) {
    throw new Error(`smoke-site failed: ${response.status} ${await response.text()}`);
  }
  console.log(`Smoke site event accepted by ${endpoint}`);
}

async function verifyPosthog(): Promise<void> {
  const client = posthogClient();
  const response = await client.post<JsonObject>("/query/", {
    query: {
      kind: "HogQLQuery",
      query: `
SELECT
  timestamp,
  event,
  distinct_id,
  properties
FROM events
WHERE event = 'squeezy_session_summary'
  AND timestamp > now() - INTERVAL 1 HOUR
ORDER BY timestamp DESC
LIMIT 20
/* smoke ${Date.now()} */
`.trim(),
    },
  });
  console.log(JSON.stringify(response, null, 2));
}

async function ensureDashboards(client: PosthogClient): Promise<Record<DashboardKey, JsonObject>> {
  const entries = await Promise.all(
    Object.entries(DASHBOARDS).map(async ([key, spec]) => [
      key,
      await ensureDashboard(client, spec.name, spec.description),
    ]),
  );
  return Object.fromEntries(entries) as Record<DashboardKey, JsonObject>;
}

async function ensureDashboard(
  client: PosthogClient,
  name: string,
  description: string,
): Promise<JsonObject> {
  const existing = await client.get<{ results?: JsonObject[] }>(
    `/dashboards/?limit=100&search=${encodeURIComponent(name)}`,
  );
  const dashboard = existing.results?.find((item) => item.name === name);
  if (dashboard) {
    return client.patch<JsonObject>(`/dashboards/${dashboard.id}/`, {
      name,
      description,
      pinned: true,
      tags: ["squeezy", "telemetry"],
    });
  }
  return client.post<JsonObject>("/dashboards/", {
    name,
    description,
    pinned: true,
    tags: ["squeezy", "telemetry"],
  });
}

async function demoteLegacyDashboard(client: PosthogClient): Promise<void> {
  const existing = await client.get<{ results?: JsonObject[] }>(
    `/dashboards/?limit=100&search=${encodeURIComponent(LEGACY_DASHBOARD_NAME)}`,
  );
  const dashboard = existing.results?.find((item) => item.name === LEGACY_DASHBOARD_NAME);
  if (dashboard?.id === undefined) {
    return;
  }
  await client.patch<JsonObject>(`/dashboards/${dashboard.id}/`, {
    name: "Squeezy - Legacy Telemetry (superseded)",
    description: "Superseded by the numbered Squeezy dashboards.",
    pinned: false,
    tags: ["squeezy", "telemetry", "legacy"],
  });
}

async function ensureInsight(
  client: PosthogClient,
  dashboardId: unknown,
  insight: InsightSpec,
): Promise<JsonObject> {
  const existing = await client.get<{ results?: JsonObject[] }>(
    `/insights/?limit=100&search=${encodeURIComponent(insight.name)}`,
  );
  const found = existing.results?.find((item) => item.name === insight.name);
  const payload = {
    name: insight.name,
    description: insight.description,
    dashboards: [dashboardId],
    tags: ["squeezy", "telemetry"],
    query: {
      kind: "DataTableNode",
      source: {
        kind: "HogQLQuery",
        query: insight.query,
      },
    },
  };
  if (found?.id !== undefined) {
    return client.patch<JsonObject>(`/insights/${found.id}/`, payload);
  }
  return client.post<JsonObject>("/insights/", payload);
}

type PosthogClient = ReturnType<typeof posthogClient>;

function posthogClient() {
  const host = trimTrailingSlash(process.env.POSTHOG_HOST || DEFAULT_POSTHOG_HOST);
  const environmentId = process.env.POSTHOG_ENVIRONMENT_ID || DEFAULT_ENVIRONMENT_ID;
  const apiKey = requiredEnv("POSTHOG_PERSONAL_API_KEY");
  const base = `${host}/api/environments/${environmentId}`;

  async function request<T>(path: string, init: RequestInit): Promise<T> {
    const response = await fetch(`${base}${path}`, {
      ...init,
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${apiKey}`,
        ...(init.headers || {}),
      },
    });
    if (!response.ok) {
      throw new Error(`${init.method || "GET"} ${path} failed: ${response.status} ${await response.text()}`);
    }
    if (response.status === 204) {
      return undefined as T;
    }
    return (await response.json()) as T;
  }

  return {
    get: <T>(path: string) => request<T>(path, { method: "GET" }),
    post: <T>(path: string, body: JsonObject) =>
      request<T>(path, { method: "POST", body: JSON.stringify(body) }),
    patch: <T>(path: string, body: JsonObject) =>
      request<T>(path, { method: "PATCH", body: JSON.stringify(body) }),
  };
}

function requiredEnv(name: string): string {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} is required`);
  }
  return value;
}

function trimTrailingSlash(value: string): string {
  return value.replace(/\/+$/, "");
}

await main();

export {};
