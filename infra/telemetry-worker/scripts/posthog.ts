const DEFAULT_POSTHOG_HOST = "https://eu.posthog.com";
const DEFAULT_ENVIRONMENT_ID = "185494";
const DASHBOARD_NAME = "Squeezy Telemetry";

type JsonObject = Record<string, unknown>;

type InsightSpec = {
  name: string;
  description: string;
  query: string;
};

const INSIGHTS: InsightSpec[] = [
  {
    name: "Squeezy DAU",
    description: "Daily anonymous unique users from app startup events.",
    query: `
SELECT
  toDate(timestamp) AS day,
  uniq(properties.user_id) AS users
FROM events
WHERE event = 'squeezy_app_started'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY day
ORDER BY day ASC
`.trim(),
  },
  {
    name: "Squeezy Version Distribution",
    description: "Recent app versions seen in telemetry.",
    query: `
SELECT
  properties.app_version AS app_version,
  count() AS events,
  uniq(properties.user_id) AS users
FROM events
WHERE event = 'squeezy_app_started'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY app_version
ORDER BY events DESC
LIMIT 20
`.trim(),
  },
  {
    name: "Squeezy OS And Arch",
    description: "Anonymous platform distribution.",
    query: `
SELECT
  concat(toString(properties.os), ' / ', toString(properties.arch)) AS platform,
  count() AS events,
  uniq(properties.user_id) AS users
FROM events
WHERE event = 'squeezy_app_started'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY platform
ORDER BY users DESC
`.trim(),
  },
  {
    name: "Squeezy Tool Calls By Status",
    description: "First-party tool-call volume by tool and outcome.",
    query: `
SELECT
  properties.tool_name AS tool_name,
  properties.tool_status AS status,
  count() AS calls,
  avg(toUInt64OrZero(toString(properties.duration_ms))) AS avg_duration_ms
FROM events
WHERE event = 'squeezy_tool_completed'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY tool_name, status
ORDER BY calls DESC
LIMIT 50
`.trim(),
  },
  {
    name: "Squeezy Turn Cost And Tokens",
    description: "Turn-level token and estimated cost counters.",
    query: `
SELECT
  toDate(timestamp) AS day,
  sum(toUInt64OrZero(toString(properties.input_tokens))) AS input_tokens,
  sum(toUInt64OrZero(toString(properties.output_tokens))) AS output_tokens,
  sum(toUInt64OrZero(toString(properties.cached_tokens))) AS cached_tokens,
  sum(toUInt64OrZero(toString(properties.estimated_usd_micros))) / 1000000 AS estimated_usd
FROM events
WHERE event = 'squeezy_turn_completed'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY day
ORDER BY day ASC
`.trim(),
  },
  {
    name: "Squeezy Failures",
    description: "Coarse anonymous failure kinds.",
    query: `
SELECT
  properties.error_kind AS error_kind,
  count() AS failures
FROM events
WHERE event = 'squeezy_failure_seen'
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY error_kind
ORDER BY failures DESC
`.trim(),
  },
  {
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
    name: "Squeezy Graph Build Performance",
    description: "AST/graph build and refresh performance counters.",
    query: `
SELECT
  event,
  properties.status AS status,
  count() AS events,
  avg(toUInt64OrZero(toString(properties.duration_ms))) AS avg_duration_ms,
  sum(toUInt64OrZero(toString(properties.supported_files))) AS supported_files,
  sum(toUInt64OrZero(toString(properties.unsupported_files))) AS unsupported_files,
  sum(toUInt64OrZero(toString(properties.rust_files))) AS rust_files
FROM events
WHERE event IN ('squeezy_graph_build_completed', 'squeezy_graph_refresh_completed')
  AND timestamp > now() - INTERVAL 30 DAY
GROUP BY event, status
ORDER BY events DESC
`.trim(),
  },
];

async function main(): Promise<void> {
  const command = process.argv[2];
  if (command === "setup-dashboard") {
    await setupDashboard();
  } else if (command === "smoke-worker") {
    await smokeWorker();
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
  bun scripts/posthog.ts verify-posthog

Environment:
  POSTHOG_PERSONAL_API_KEY  Required for setup-dashboard and verify-posthog.
  POSTHOG_ENVIRONMENT_ID    Defaults to ${DEFAULT_ENVIRONMENT_ID}.
  POSTHOG_HOST              Defaults to ${DEFAULT_POSTHOG_HOST}.
  TELEMETRY_ENDPOINT        Required for smoke-worker, e.g. https://squeezy-telemetry.esqueezy.workers.dev/v1/batch.
`);
}

async function setupDashboard(): Promise<void> {
  const client = posthogClient();
  const dashboard = await ensureDashboard(client);
  console.log(`Dashboard: ${dashboard.name} (${dashboard.id})`);

  for (const insight of INSIGHTS) {
    const created = await ensureInsight(client, dashboard.id, insight);
    console.log(`Insight: ${created.name} (${created.id})`);
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
        event: "squeezy_app_started",
        timestamp_ms: now,
        event_sequence: 1,
        properties: {
          provider: "open_ai",
          model_family: "gpt",
        },
      },
      {
        event: "squeezy_tool_completed",
        timestamp_ms: now + 1,
        event_sequence: 2,
        properties: {
          turn_index: 1,
          tool_sequence: 1,
          tool_name: "grep",
          tool_family: "search",
          tool_status: "success",
          duration_ms: 12,
          files_scanned: 3,
          bytes_read: 128,
          output_bytes: 64,
          matches_returned: 2,
        },
      },
      {
        event: "squeezy_turn_completed",
        timestamp_ms: now + 2,
        event_sequence: 3,
        properties: {
          turn_index: 1,
          provider: "open_ai",
          model_family: "gpt",
          status: "success",
          tool_calls: 1,
          input_tokens: 100,
          output_tokens: 20,
          cached_tokens: 10,
          estimated_usd_micros: 1,
          receipt_stub_hits: 0,
          negative_receipt_hits: 0,
          budget_denials: 0,
        },
      },
      {
        event: "squeezy_failure_seen",
        timestamp_ms: now + 3,
        event_sequence: 4,
        properties: {
          error_kind: "unknown",
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

async function verifyPosthog(): Promise<void> {
  const client = posthogClient();
  const response = await client.post<JsonObject>("/query/", {
    query: {
      kind: "HogQLQuery",
      query: `
SELECT
  event,
  count() AS events,
  max(timestamp) AS latest
FROM events
WHERE event LIKE 'squeezy_%'
  AND timestamp > now() - INTERVAL 1 HOUR
GROUP BY event
ORDER BY event ASC
/* smoke ${Date.now()} */
`.trim(),
    },
  });
  console.log(JSON.stringify(response, null, 2));
}

async function ensureDashboard(client: PosthogClient): Promise<JsonObject> {
  const existing = await client.get<{ results?: JsonObject[] }>(
    `/dashboards/?limit=100&search=${encodeURIComponent(DASHBOARD_NAME)}`,
  );
  const dashboard = existing.results?.find((item) => item.name === DASHBOARD_NAME);
  if (dashboard) {
    return dashboard;
  }
  return client.post<JsonObject>("/dashboards/", {
    name: DASHBOARD_NAME,
    description: "Anonymous Squeezy product telemetry: usage, reliability, cost, and performance.",
    pinned: true,
    tags: ["squeezy", "telemetry"],
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
