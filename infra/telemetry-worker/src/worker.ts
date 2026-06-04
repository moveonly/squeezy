const MAX_BODY_BYTES = 64 * 1024;
const MAX_SITE_BODY_BYTES = 16 * 1024;
const MAX_FEEDBACK_BODY_BYTES = 32 * 1024;
const MAX_FEEDBACK_MESSAGE_BYTES = 16 * 1024;
const MAX_REPORT_BYTES = 2 * 1024 * 1024;
const MAX_EVENTS = 50;
const MAX_PRODUCT_PROPERTIES = 128;
const MAX_COUNT_MAP_ENTRIES = 16;
const SCHEMA_VERSION = 1;
const DEFAULT_POSTHOG_HOST = "https://eu.i.posthog.com";
const PRODUCT_EVENT_RE = /^squeezy_[a-z0-9_]{1,96}$/;
const SAFE_TOKEN_RE = /^[A-Za-z0-9._+:-]+$/;
const SAFE_PROPERTY_KEY_RE = /^[A-Za-z0-9_]{1,80}$/;
const TRACE_ID_RE = /^[0-9a-f]{32}$/;
const SPAN_ID_RE = /^[0-9a-f]{16}$/;

const TEXT_ENCODER = new TextEncoder();
function utf8ByteLength(text: string): number {
  return TEXT_ENCODER.encode(text).length;
}

interface Env {
  POSTHOG_PROJECT_TOKEN: string;
  POSTHOG_HOST?: string;
  REPORT_BUCKET?: R2Bucket;
}

type JsonObject = Record<string, unknown>;
type SanitizedTelemetryEvent = {
  event: string;
  timestampMs: number;
  eventSequence: number;
  properties: JsonObject;
};

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

const SITE_EVENT_NAMES = new Set([
  "squeezy_site_page_view",
  "squeezy_site_cta_clicked",
  "squeezy_site_outbound_clicked",
]);
const FEEDBACK_SOURCES = new Set(["cli", "tui"]);
const SITE_REFERRER_KINDS = new Set(["none", "internal", "search", "social", "external"]);
const SITE_TARGET_KINDS = new Set(["internal", "github", "release", "docs", "install", "other"]);

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/v1/site" && request.method === "OPTIONS") {
      return siteCorsResponse(null, { status: 204 });
    }
    if (request.method !== "POST") {
      return jsonResponse(405, { error: "method_not_allowed" });
    }
    if (!env.POSTHOG_PROJECT_TOKEN) {
      return jsonResponse(500, { error: "telemetry_not_configured" });
    }
    if (url.pathname === "/v1/site") {
      return handleSiteEvent(request, env);
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

    let text: string;
    try {
      text = await boundedText(request, MAX_BODY_BYTES);
    } catch {
      return jsonResponse(413, { error: "body_too_large" });
    }

    let batch: JsonObject;
    let events: SanitizedTelemetryEvent[];
    try {
      batch = JSON.parse(text) as JsonObject;
      validateBatch(batch);
      events = (batch.events as JsonObject[]).map(sanitizeEvent);
    } catch {
      return jsonResponse(400, { error: "invalid_batch" });
    }

    const response = await sendPostHogBatch(
      env,
      events.map((event) => ({
        event: event.event,
        timestamp: new Date(event.timestampMs).toISOString(),
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
          event_sequence: event.eventSequence,
          ...event.properties,
        },
      })),
    );
    if (!response.ok) {
      return jsonResponse(502, { error: "posthog_rejected" });
    }
    return new Response(null, { status: 204 });
  },
};

async function handleSiteEvent(request: Request, env: Env): Promise<Response> {
  let text: string;
  let event: JsonObject;
  try {
    text = await boundedText(request, MAX_SITE_BODY_BYTES);
    event = JSON.parse(text) as JsonObject;
    validateSiteEvent(event);
  } catch (error) {
    if (error instanceof Error && error.message === "body_too_large") {
      return siteCorsResponse(JSON.stringify({ error: "body_too_large" }), {
        status: 413,
        headers: { "Content-Type": "application/json" },
      });
    }
    return siteCorsResponse(JSON.stringify({ error: "invalid_site_event" }), {
      status: 400,
      headers: { "Content-Type": "application/json" },
    });
  }

  const response = await sendPostHogEvent(env, {
    event: event.event,
    timestamp: new Date(event.timestamp_ms as number).toISOString(),
    properties: {
      distinct_id: event.visitor_id,
      $process_person_profile: false,
      schema_version: event.schema_version,
      visitor_id: event.visitor_id,
      session_id: event.session_id,
      path: event.path,
      referrer_kind: event.referrer_kind,
      cta_id: event.cta_id,
      target_kind: event.target_kind,
      utm_source: event.utm_source,
      utm_medium: event.utm_medium,
      utm_campaign: event.utm_campaign,
    },
  });
  if (!response.ok) {
    return siteCorsResponse(JSON.stringify({ error: "posthog_rejected" }), {
      status: 502,
      headers: { "Content-Type": "application/json" },
    });
  }
  return siteCorsResponse(null, { status: 204 });
}

async function handleFeedback(request: Request, env: Env): Promise<Response> {
  let text: string;
  let feedback: JsonObject;
  try {
    text = await boundedText(request, MAX_FEEDBACK_BODY_BYTES);
    feedback = JSON.parse(text) as JsonObject;
    validateFeedback(feedback);
  } catch (error) {
    if (error instanceof Error && error.message === "body_too_large") {
      return jsonResponse(413, { error: "body_too_large" });
    }
    return jsonResponse(400, { error: "invalid_feedback" });
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
  } catch {
    return jsonResponse(400, { error: "invalid_report" });
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
    sanitizeEvent(event as JsonObject);
  }
}

function validateSiteEvent(event: JsonObject): void {
  assertPlainObject(event, "site_event");
  assertKeys(event, "site_event", [
    "schema_version",
    "visitor_id",
    "session_id",
    "timestamp_ms",
    "event",
    "path",
    "referrer_kind",
    "cta_id",
    "target_kind",
    "utm_source",
    "utm_medium",
    "utm_campaign",
  ]);
  if (event.schema_version !== SCHEMA_VERSION) {
    throw new Error("unsupported schema version");
  }
  assertUuid(event.visitor_id, "visitor_id");
  assertUuid(event.session_id, "session_id");
  assertU64(event.timestamp_ms, "timestamp_ms");
  const now = Date.now();
  if (
    (event.timestamp_ms as number) < now - 1000 * 60 * 60 * 24 * 30 ||
    (event.timestamp_ms as number) > now + 1000 * 60 * 10
  ) {
    throw new Error("timestamp_ms outside accepted window");
  }
  if (typeof event.event !== "string" || !SITE_EVENT_NAMES.has(event.event)) {
    throw new Error("unknown site event name");
  }
  assertSitePath(event.path, "path");
  if (typeof event.referrer_kind !== "string" || !SITE_REFERRER_KINDS.has(event.referrer_kind)) {
    throw new Error("invalid referrer_kind");
  }
  assertOptionalSiteToken(event.cta_id, "cta_id", 80);
  if (
    event.target_kind !== undefined &&
    (typeof event.target_kind !== "string" || !SITE_TARGET_KINDS.has(event.target_kind))
  ) {
    throw new Error("invalid target_kind");
  }
  assertOptionalSiteToken(event.utm_source, "utm_source", 80);
  assertOptionalSiteToken(event.utm_medium, "utm_medium", 80);
  assertOptionalSiteToken(event.utm_campaign, "utm_campaign", 80);
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
  if (utf8ByteLength(feedback.message as string) !== feedback.message_bytes) {
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

function sanitizeEvent(event: JsonObject): SanitizedTelemetryEvent {
  assertPlainObject(event, "event");
  assertKeys(event, "event", ["event", "timestamp_ms", "event_sequence", "properties"]);
  if (!isProductEventName(event.event)) {
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
  return {
    event: event.event as string,
    timestampMs: event.timestamp_ms as number,
    eventSequence: event.event_sequence as number,
    properties: sanitizeProperties(event.properties as JsonObject),
  };
}

function isProductEventName(value: unknown): value is string {
  return typeof value === "string" && PRODUCT_EVENT_RE.test(value);
}

function sanitizeProperties(properties: JsonObject): JsonObject {
  assertPlainObject(properties, "properties");
  const sanitized: JsonObject = {};
  for (const [key, value] of Object.entries(properties)) {
    if (Object.keys(sanitized).length >= MAX_PRODUCT_PROPERTIES) {
      break;
    }
    if (!SAFE_PROPERTY_KEY_RE.test(key)) {
      continue;
    }
    const safe = sanitizePropertyValue(value, key);
    if (safe === undefined) {
      continue;
    }
    sanitized[key] = safe;
  }
  return sanitized;
}

function sanitizePropertyValue(value: unknown, label: string): unknown {
  try {
    if (Number.isSafeInteger(value) && (value as number) >= 0) {
      return value;
    }
    if (typeof value === "boolean") {
      return value;
    }
    if (typeof value === "string") {
      if (label === "trace_id") {
        return TRACE_ID_RE.test(value) ? value : undefined;
      }
      if (label === "span_id") {
        return SPAN_ID_RE.test(value) ? value : undefined;
      }
      assertString(value, label, 1, 128);
      return value;
    }
    if (value && typeof value === "object" && !Array.isArray(value)) {
      assertCountMap(value, label);
      return value;
    }
  } catch {
    return undefined;
  }
  return undefined;
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
  if (!SAFE_TOKEN_RE.test(value)) {
    throw new Error(`${label} has invalid characters`);
  }
}

function assertSitePath(value: unknown, label: string): void {
  if (typeof value !== "string" || value.length < 1 || value.length > 160) {
    throw new Error(`${label} must be a bounded path`);
  }
  if (!value.startsWith("/") || value.startsWith("//") || /[\u0000-\u001f]/.test(value)) {
    throw new Error(`${label} must be a site-local path`);
  }
  if (!/^[A-Za-z0-9/_?.=&%#+:-]+$/.test(value)) {
    throw new Error(`${label} has invalid characters`);
  }
}

function assertOptionalSiteToken(value: unknown, label: string, max: number): void {
  if (value === undefined) {
    return;
  }
  if (typeof value !== "string" || value.length < 1 || value.length > max) {
    throw new Error(`${label} must be a bounded token`);
  }
  if (!/^[A-Za-z0-9._:-]+$/.test(value)) {
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
  if (utf8ByteLength(value) > maxBytes) {
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

function assertBoolean(value: unknown, label: string): void {
  if (typeof value !== "boolean") {
    throw new Error(`${label} must be a boolean`);
  }
}

function assertCountMap(value: unknown, label: string): void {
  assertPlainObject(value, label);
  const entries = Object.entries(value);
  if (entries.length === 0 || entries.length > MAX_COUNT_MAP_ENTRIES) {
    throw new Error(`${label} must be a bounded count map`);
  }
  for (const [key, count] of entries) {
    assertString(key, `${label}.key`, 1, 128);
    assertU64(count, `${label}.${key}`);
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
  if (utf8ByteLength(text) > maxBytes) {
    throw new Error("body_too_large");
  }
  return text;
}

function siteCorsResponse(body: BodyInit | null, init: ResponseInit = {}): Response {
  const headers = new Headers(init.headers);
  headers.set("Access-Control-Allow-Origin", "https://squeezyagent.com");
  headers.set("Access-Control-Allow-Methods", "POST, OPTIONS");
  headers.set("Access-Control-Allow-Headers", "content-type");
  headers.set("Access-Control-Max-Age", "86400");
  return new Response(body, { ...init, headers });
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
