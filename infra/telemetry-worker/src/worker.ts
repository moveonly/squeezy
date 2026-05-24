const MAX_BODY_BYTES = 64 * 1024;
const MAX_FEEDBACK_BODY_BYTES = 32 * 1024;
const MAX_FEEDBACK_MESSAGE_BYTES = 16 * 1024;
const MAX_REPORT_BYTES = 2 * 1024 * 1024;
const MAX_EVENTS = 50;
const SCHEMA_VERSION = 1;
const DEFAULT_POSTHOG_HOST = "https://eu.i.posthog.com";

interface Env {
  POSTHOG_PROJECT_TOKEN: string;
  POSTHOG_HOST?: string;
  REPORT_BUCKET?: R2Bucket;
}

type JsonObject = Record<string, unknown>;

interface R2Bucket {
  put(
    key: string,
    value: ArrayBuffer,
    options?: {
      httpMetadata?: { contentType?: string };
      customMetadata?: Record<string, string>;
    },
  ): Promise<unknown>;
}

const EVENT_NAMES = new Set([
  "squeezy_app_started",
  "squeezy_turn_completed",
  "squeezy_tool_completed",
  "squeezy_graph_build_completed",
  "squeezy_graph_refresh_completed",
  "squeezy_failure_seen",
]);
const FEEDBACK_SOURCES = new Set(["cli", "tui"]);

const PROVIDERS = new Set(["open_ai", "anthropic", "google", "azure_open_ai", "bedrock", "ollama"]);
const MODEL_FAMILIES = new Set(["gpt", "claude", "gemini", "bedrock", "ollama", "other"]);
const TOOL_NAMES = new Set([
  "glob",
  "grep",
  "read_file",
  "read_tool_output",
  "write_file",
  "shell",
  "webfetch",
  "websearch",
  "graph",
  "ast",
  "other",
]);
const TOOL_FAMILIES = new Set(["search", "read", "write", "shell", "web", "graph", "ast", "other"]);
const TOOL_STATUSES = new Set(["success", "error", "denied", "stale", "cancelled"]);
const REFRESH_KINDS = new Set(["cold", "incremental"]);
const GRAPH_SEQUENCE_SCOPES = new Set(["one_shot", "repeated"]);
const OUTCOME_STATUSES = new Set(["success", "error", "cancelled", "skipped"]);
const ERROR_KINDS = new Set(["provider", "tool", "permission", "budget", "graph", "io", "config", "unknown"]);

const PROPERTY_SCHEMAS: Record<string, "u64" | Set<string>> = {
  turn_index: "u64",
  tool_sequence: "u64",
  provider: PROVIDERS,
  model_family: MODEL_FAMILIES,
  tool_name: TOOL_NAMES,
  tool_family: TOOL_FAMILIES,
  tool_status: TOOL_STATUSES,
  duration_ms: "u64",
  tool_calls: "u64",
  files_scanned: "u64",
  rust_files: "u64",
  supported_files: "u64",
  unsupported_files: "u64",
  unknown_files: "u64",
  files_changed: "u64",
  files_parsed: "u64",
  bytes_read: "u64",
  bytes_parsed: "u64",
  output_bytes: "u64",
  matches_returned: "u64",
  symbols: "u64",
  edges: "u64",
  input_tokens: "u64",
  output_tokens: "u64",
  cached_tokens: "u64",
  estimated_usd_micros: "u64",
  receipt_stub_hits: "u64",
  negative_receipt_hits: "u64",
  budget_denials: "u64",
  refresh_kind: REFRESH_KINDS,
  graph_sequence_scope: GRAPH_SEQUENCE_SCOPES,
  status: OUTCOME_STATUSES,
  error_kind: ERROR_KINDS,
};

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    if (request.method !== "POST") {
      return jsonResponse(405, { error: "method_not_allowed" });
    }
    const url = new URL(request.url);
    if (!env.POSTHOG_PROJECT_TOKEN) {
      return jsonResponse(500, { error: "telemetry_not_configured" });
    }
    if (url.pathname === "/v1/feedback") {
      return handleFeedback(request, env);
    }
    if (url.pathname === "/v1/report") {
      return handleReport(request, env);
    }
    if (url.pathname !== "/v1/batch") {
      return jsonResponse(404, { error: "not_found" });
    }

    const contentLength = Number(request.headers.get("content-length") || "0");
    if (contentLength > MAX_BODY_BYTES) {
      return jsonResponse(413, { error: "body_too_large" });
    }

    const text = await request.text();
    if (new TextEncoder().encode(text).length > MAX_BODY_BYTES) {
      return jsonResponse(413, { error: "body_too_large" });
    }

    let batch: JsonObject;
    try {
      batch = JSON.parse(text) as JsonObject;
      validateBatch(batch);
    } catch (error) {
      return jsonResponse(400, {
        error: "invalid_batch",
        detail: String((error as Error).message || error),
      });
    }

    const events = batch.events as JsonObject[];
    const response = await sendPostHogBatch(
      env,
      events.map((event) => ({
        event: event.event,
        timestamp: new Date(event.timestamp_ms as number).toISOString(),
        properties: {
          distinct_id: batch.user_id,
          $process_person_profile: false,
          schema_version: batch.schema_version,
          user_id: batch.user_id,
          install_id: batch.install_id,
          session_id: batch.session_id,
          app_version: batch.app_version,
          os: batch.os,
          arch: batch.arch,
          event_sequence: event.event_sequence,
          ...(event.properties as JsonObject),
        },
      })),
    );
    if (!response.ok) {
      return jsonResponse(502, { error: "posthog_rejected" });
    }
    return new Response(null, { status: 204 });
  },
};

async function handleFeedback(request: Request, env: Env): Promise<Response> {
  let text: string;
  let feedback: JsonObject;
  try {
    text = await boundedText(request, MAX_FEEDBACK_BODY_BYTES);
    feedback = JSON.parse(text) as JsonObject;
    validateFeedback(feedback);
  } catch (error) {
    if (String((error as Error).message || error) === "body_too_large") {
      return jsonResponse(413, { error: "body_too_large" });
    }
    return jsonResponse(400, {
      error: "invalid_feedback",
      detail: String((error as Error).message || error),
    });
  }
  const response = await sendPostHogEvent(env, {
    event: "squeezy_feedback_submitted",
    timestamp: new Date(feedback.timestamp_ms as number).toISOString(),
    properties: {
      distinct_id: feedback.user_id,
      $process_person_profile: false,
      schema_version: feedback.schema_version,
      feedback_id: feedback.feedback_id,
      user_id: feedback.user_id,
      install_id: feedback.install_id,
      session_id: feedback.session_id,
      app_version: feedback.app_version,
      os: feedback.os,
      arch: feedback.arch,
      source: feedback.source,
      message: feedback.message,
      message_bytes: feedback.message_bytes,
      redactions: feedback.redactions,
    },
  });
  if (!response.ok) {
    return jsonResponse(502, { error: "posthog_rejected" });
  }
  return jsonResponse(201, { id: feedback.feedback_id, feedback_id: feedback.feedback_id });
}

async function handleReport(request: Request, env: Env): Promise<Response> {
  if (!env.REPORT_BUCKET) {
    return jsonResponse(500, { error: "report_storage_not_configured" });
  }
  const contentLength = Number(request.headers.get("content-length") || "0");
  if (contentLength > MAX_REPORT_BYTES) {
    return jsonResponse(413, { error: "report_too_large" });
  }
  let metadata: ReportMetadata;
  try {
    metadata = validateReportHeaders(request.headers);
  } catch (error) {
    return jsonResponse(400, {
      error: "invalid_report",
      detail: String((error as Error).message || error),
    });
  }
  const body = await request.arrayBuffer();
  if (body.byteLength > MAX_REPORT_BYTES) {
    return jsonResponse(413, { error: "report_too_large" });
  }
  if (body.byteLength !== metadata.archiveBytes) {
    return jsonResponse(400, { error: "archive_size_mismatch" });
  }
  const key = `reports/${metadata.reportId}.tar`;
  await env.REPORT_BUCKET.put(key, body, {
    httpMetadata: { contentType: "application/x-tar" },
    customMetadata: {
      report_id: metadata.reportId,
      session_id: metadata.sessionId,
      source: metadata.source,
      app_version: metadata.appVersion,
      os: metadata.os,
      arch: metadata.arch,
      archive_bytes: String(metadata.archiveBytes),
      redactions: String(metadata.redactions),
      sections: metadata.sections.join(","),
    },
  });
  const response = await sendPostHogEvent(env, {
    event: "squeezy_report_submitted",
    timestamp: new Date().toISOString(),
    properties: {
      distinct_id: metadata.userId,
      $process_person_profile: false,
      schema_version: SCHEMA_VERSION,
      report_id: metadata.reportId,
      user_id: metadata.userId,
      install_id: metadata.installId,
      session_id: metadata.clientSessionId,
      reported_session_id: metadata.sessionId,
      app_version: metadata.appVersion,
      os: metadata.os,
      arch: metadata.arch,
      source: metadata.source,
      archive_bytes: metadata.archiveBytes,
      redactions: metadata.redactions,
      sections: metadata.sections.join(","),
      r2_key: key,
    },
  });
  if (!response.ok) {
    return jsonResponse(502, { error: "posthog_rejected" });
  }
  return jsonResponse(201, { id: metadata.reportId, report_id: metadata.reportId });
}

function validateBatch(batch: JsonObject): void {
  assertPlainObject(batch, "batch");
  assertKeys(batch, "batch", [
    "schema_version",
    "user_id",
    "install_id",
    "session_id",
    "app_version",
    "os",
    "arch",
    "events",
  ]);
  if (batch.schema_version !== SCHEMA_VERSION) {
    throw new Error("unsupported schema_version");
  }
  assertUuid(batch.user_id, "user_id");
  assertUuid(batch.install_id, "install_id");
  if (batch.user_id !== batch.install_id) {
    throw new Error("user_id must match install_id");
  }
  assertUuid(batch.session_id, "session_id");
  assertString(batch.app_version, "app_version", 1, 64);
  assertString(batch.os, "os", 1, 32);
  assertString(batch.arch, "arch", 1, 32);
  if (!Array.isArray(batch.events) || batch.events.length === 0 || batch.events.length > MAX_EVENTS) {
    throw new Error("events must be a non-empty bounded array");
  }
  for (const event of batch.events) {
    validateEvent(event as JsonObject);
  }
}

function validateFeedback(feedback: JsonObject): void {
  assertPlainObject(feedback, "feedback");
  assertKeys(feedback, "feedback", [
    "schema_version",
    "feedback_id",
    "user_id",
    "install_id",
    "session_id",
    "app_version",
    "os",
    "arch",
    "source",
    "timestamp_ms",
    "message",
    "message_bytes",
    "redactions",
  ]);
  if (feedback.schema_version !== SCHEMA_VERSION) {
    throw new Error("unsupported schema_version");
  }
  assertUuid(feedback.feedback_id, "feedback_id");
  assertUuid(feedback.user_id, "user_id");
  assertUuid(feedback.install_id, "install_id");
  if (feedback.user_id !== feedback.install_id) {
    throw new Error("user_id must match install_id");
  }
  assertUuid(feedback.session_id, "session_id");
  assertString(feedback.app_version, "app_version", 1, 64);
  assertString(feedback.os, "os", 1, 32);
  assertString(feedback.arch, "arch", 1, 32);
  if (typeof feedback.source !== "string" || !FEEDBACK_SOURCES.has(feedback.source)) {
    throw new Error("invalid source");
  }
  assertU64(feedback.timestamp_ms, "timestamp_ms");
  assertBoundedText(feedback.message, "message", 1, MAX_FEEDBACK_MESSAGE_BYTES);
  assertU64(feedback.message_bytes, "message_bytes");
  if (new TextEncoder().encode(feedback.message as string).length !== feedback.message_bytes) {
    throw new Error("message_bytes mismatch");
  }
  assertU64(feedback.redactions, "redactions");
}

interface ReportMetadata {
  reportId: string;
  sessionId: string;
  source: string;
  appVersion: string;
  os: string;
  arch: string;
  installId: string;
  userId: string;
  clientSessionId: string;
  archiveBytes: number;
  redactions: number;
  sections: string[];
}

function validateReportHeaders(headers: Headers): ReportMetadata {
  const schema = requiredHeader(headers, "x-squeezy-schema-version");
  if (schema !== String(SCHEMA_VERSION)) {
    throw new Error("unsupported schema_version");
  }
  const reportId = requiredHeader(headers, "x-squeezy-report-id");
  const sessionId = requiredHeader(headers, "x-squeezy-session-id");
  const source = requiredHeader(headers, "x-squeezy-source");
  const appVersion = requiredHeader(headers, "x-squeezy-app-version");
  const os = requiredHeader(headers, "x-squeezy-os");
  const arch = requiredHeader(headers, "x-squeezy-arch");
  const installId = requiredHeader(headers, "x-squeezy-install-id");
  const userId = requiredHeader(headers, "x-squeezy-user-id");
  const clientSessionId = requiredHeader(headers, "x-squeezy-client-session-id");
  const archiveBytes = headerU64(headers, "x-squeezy-archive-bytes");
  const redactions = headerU64(headers, "x-squeezy-redactions");
  const sections = requiredHeader(headers, "x-squeezy-sections")
    .split(",")
    .filter((section) => section.length > 0);
  assertUuid(reportId, "report_id");
  assertString(sessionId, "session_id", 1, 128);
  if (!FEEDBACK_SOURCES.has(source)) throw new Error("invalid source");
  assertString(appVersion, "app_version", 1, 64);
  assertString(os, "os", 1, 32);
  assertString(arch, "arch", 1, 32);
  assertUuid(installId, "install_id");
  assertUuid(userId, "user_id");
  if (userId !== installId) throw new Error("user_id must match install_id");
  assertUuid(clientSessionId, "client_session_id");
  if (archiveBytes <= 0 || archiveBytes > MAX_REPORT_BYTES) throw new Error("archive_bytes out of range");
  if (sections.length === 0 || sections.length > 32) throw new Error("sections out of range");
  for (const section of sections) assertString(section, "section", 1, 64);
  return {
    reportId,
    sessionId,
    source,
    appVersion,
    os,
    arch,
    installId,
    userId,
    clientSessionId,
    archiveBytes,
    redactions,
    sections,
  };
}

function validateEvent(event: JsonObject): void {
  assertPlainObject(event, "event");
  assertKeys(event, "event", ["event", "timestamp_ms", "event_sequence", "properties"]);
  if (typeof event.event !== "string" || !EVENT_NAMES.has(event.event)) {
    throw new Error("unknown event name");
  }
  assertU64(event.timestamp_ms, "timestamp_ms");
  assertU64(event.event_sequence, "event_sequence");
  const now = Date.now();
  if (
    (event.timestamp_ms as number) < now - 1000 * 60 * 60 * 24 * 30 ||
    (event.timestamp_ms as number) > now + 1000 * 60 * 10
  ) {
    throw new Error("timestamp_ms outside accepted window");
  }
  validateProperties(event.properties as JsonObject);
}

function validateProperties(properties: JsonObject): void {
  assertPlainObject(properties, "properties");
  for (const [key, value] of Object.entries(properties)) {
    const schema = PROPERTY_SCHEMAS[key];
    if (!schema) {
      throw new Error(`unknown property: ${key}`);
    }
    if (schema === "u64") {
      assertU64(value, key);
    } else if (typeof value !== "string" || !schema.has(value)) {
      throw new Error(`invalid enum value for ${key}`);
    }
  }
}

function assertKeys(object: JsonObject, label: string, allowed: string[]): void {
  const allowedSet = new Set(allowed);
  for (const key of Object.keys(object)) {
    if (!allowedSet.has(key)) {
      throw new Error(`unknown ${label} field: ${key}`);
    }
  }
}

function assertPlainObject(value: unknown, label: string): asserts value is JsonObject {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${label} must be an object`);
  }
}

function assertString(value: unknown, label: string, min: number, max: number): void {
  if (typeof value !== "string" || value.length < min || value.length > max) {
    throw new Error(`${label} must be a bounded string`);
  }
  if (!/^[A-Za-z0-9._+:-]+$/.test(value)) {
    throw new Error(`${label} has invalid characters`);
  }
}

function assertBoundedText(value: unknown, label: string, min: number, maxBytes: number): void {
  if (typeof value !== "string" || value.length < min) {
    throw new Error(`${label} must be a bounded string`);
  }
  if (value.includes("\u0000")) {
    throw new Error(`${label} must not contain NUL bytes`);
  }
  if (new TextEncoder().encode(value).length > maxBytes) {
    throw new Error(`${label} is too large`);
  }
}

function assertUuid(value: unknown, label: string): void {
  if (
    typeof value !== "string" ||
    !/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value)
  ) {
    throw new Error(`${label} must be a uuid`);
  }
}

function assertU64(value: unknown, label: string): void {
  if (!Number.isSafeInteger(value) || (value as number) < 0) {
    throw new Error(`${label} must be a safe non-negative integer`);
  }
}

function requiredHeader(headers: Headers, name: string): string {
  const value = headers.get(name);
  if (!value) throw new Error(`missing ${name}`);
  return value;
}

function headerU64(headers: Headers, name: string): number {
  const value = Number(requiredHeader(headers, name));
  assertU64(value, name);
  return value;
}

async function boundedText(request: Request, maxBytes: number): Promise<string> {
  const contentLength = Number(request.headers.get("content-length") || "0");
  if (contentLength > maxBytes) {
    throw new Error("body_too_large");
  }
  const text = await request.text();
  if (new TextEncoder().encode(text).length > maxBytes) {
    throw new Error("body_too_large");
  }
  return text;
}

async function sendPostHogEvent(env: Env, event: JsonObject): Promise<Response> {
  return sendPostHogBatch(env, [event]);
}

async function sendPostHogBatch(env: Env, batch: JsonObject[]): Promise<Response> {
  const posthogHost = normalizePosthogHost(env.POSTHOG_HOST || DEFAULT_POSTHOG_HOST);
  return fetch(`${posthogHost}/batch/`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      api_key: env.POSTHOG_PROJECT_TOKEN,
      batch,
    }),
  });
}

function normalizePosthogHost(value: string): string {
  const url = new URL(value);
  if (url.protocol !== "https:") {
    throw new Error("POSTHOG_HOST must be https");
  }
  return url.origin;
}

function jsonResponse(status: number, body: JsonObject): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}
