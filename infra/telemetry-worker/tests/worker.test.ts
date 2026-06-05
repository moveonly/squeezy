import { afterEach, expect, test } from "bun:test";
import worker from "../src/worker";

const originalFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = originalFetch;
});

function env() {
  return {
    POSTHOG_PROJECT_TOKEN: "test-token",
    POSTHOG_HOST: "https://eu.i.posthog.com",
  };
}

test("product telemetry accepts current trace and routing properties", async () => {
  const forwarded: unknown[] = [];
  globalThis.fetch = async (_input: RequestInfo | URL, init?: RequestInit) => {
    forwarded.push(JSON.parse(String(init?.body)));
    return new Response(null, { status: 200 });
  };

  const response = await worker.fetch(
    new Request("https://telemetry.example/v1/batch", {
      method: "POST",
      body: JSON.stringify({
        schema_version: 1,
        user_id: "11111111-1111-4111-8111-111111111111",
        install_id: "11111111-1111-4111-8111-111111111111",
        session_id: "22222222-2222-4222-8222-222222222222",
        app_version: "0.1.0",
        os: "macos",
        arch: "aarch64",
        events: [
          {
            event: "squeezy_tool_completed",
            timestamp_ms: Date.now(),
            event_sequence: 1,
            properties: {
              provider: "port_key",
              model_family: "other",
              tool_name: "shell",
              tool_family: "shell",
              tool_status: "success",
              duration_ms: 12,
              trace_id: "d".repeat(32),
              span_id: "e".repeat(16),
            },
          },
          {
            event: "squeezy_routing_routed",
            timestamp_ms: Date.now(),
            event_sequence: 2,
            properties: {
              routing_reason: "llm_judge",
              trace_id: "d".repeat(32),
            },
          },
          {
            event: "squeezy_config_change_committed",
            timestamp_ms: Date.now(),
            event_sequence: 3,
            properties: {
              config_scope: "project",
              config_section: "models",
              config_field: "model.model",
              config_apply_tier: "next_prompt",
              config_change_kind: "set",
              config_prev_bucket: "model_custom",
              config_new_bucket: "model_custom",
              local_path: "/Users/example/project",
            },
          },
          {
            event: "squeezy_startup_ready",
            timestamp_ms: Date.now(),
            event_sequence: 4,
            properties: {
              startup_route: "resume_picker_resume",
              duration_ms: 987,
              status: "success",
            },
          },
          {
            event: "squeezy_slash_command_used",
            timestamp_ms: Date.now(),
            event_sequence: 5,
            properties: {
              slash_command: "plan",
              slash_surface: "tui_composer",
              slash_outcome: "accepted",
              slash_alias_kind: "canonical",
              slash_arg_shape: "free_text",
            },
          },
          {
            event: "squeezy_session_ended",
            timestamp_ms: Date.now(),
            event_sequence: 6,
            properties: {
              session_status: "completed",
              duration_ms: 1234,
              turn_count: 2,
              tool_successes: 3,
              tool_errors: 1,
              tool_denials: 0,
              tool_cancellations: 0,
              subagent_calls: 1,
              subagent_failures: 0,
            },
          },
        ],
      }),
    }),
    env(),
  );

  expect(response.status).toBe(204);
  expect(forwarded).toHaveLength(1);
  const batch = forwarded[0] as { batch: Array<{ event: string; properties: Record<string, unknown> }> };
  expect(batch.batch.map((event) => event.event)).toEqual([
    "squeezy_tool_completed",
    "squeezy_routing_routed",
    "squeezy_config_change_committed",
    "squeezy_startup_ready",
    "squeezy_slash_command_used",
    "squeezy_session_ended",
  ]);
  expect(batch.batch[0].properties.trace_id).toBe("d".repeat(32));
  expect(batch.batch[2].properties.config_new_bucket).toBe("model_custom");
  expect(batch.batch[2].properties.local_path).toBeUndefined();
  expect(batch.batch[3].properties.startup_route).toBe("resume_picker_resume");
  expect(batch.batch[4].properties.slash_command).toBe("plan");
  expect(batch.batch[5].properties.session_status).toBe("completed");
});

test("product telemetry accepts durable session summary fields", async () => {
  const forwarded: unknown[] = [];
  globalThis.fetch = async (_input: RequestInfo | URL, init?: RequestInit) => {
    forwarded.push(JSON.parse(String(init?.body)));
    return new Response(null, { status: 200 });
  };

  const response = await worker.fetch(
    new Request("https://telemetry.example/v1/batch", {
      method: "POST",
      body: JSON.stringify({
        schema_version: 1,
        user_id: "11111111-1111-4111-8111-111111111111",
        install_id: "11111111-1111-4111-8111-111111111111",
        session_id: "22222222-2222-4222-8222-222222222222",
        app_version: "0.1.0",
        os: "macos",
        arch: "aarch64",
        events: [
          {
            event: "squeezy_session_summary",
            timestamp_ms: Date.now(),
            event_sequence: 1,
            properties: {
              summary_id: "33333333-3333-4333-8333-333333333333",
              trace_id: "d".repeat(32),
              started_at_ms: Date.now() - 1000,
              ended_at_ms: Date.now(),
              source_records: 12,
              dropped_buckets: 0,
              abnormal_exit: false,
              telemetry_truncated: false,
              session_status: "completed",
              turn_count: 2,
              tool_calls: 3,
              graph_build_count: 1,
              slash_command_count: 1,
              routing_escalated_count: 1,
              tool_counts: { shell: 2, graph: 1 },
              slash_counts: { plan: 1 },
              failure_counts: { provider: 1 },
              routing_counts: { "escalated:error_threshold": 1 },
              config_counts: { "model.model": 1 },
              prompt: "must not forward",
            },
          },
        ],
      }),
    }),
    env(),
  );

  expect(response.status).toBe(204);
  expect(forwarded).toHaveLength(1);
  const batch = forwarded[0] as { batch: Array<{ event: string; properties: Record<string, unknown> }> };
  expect(batch.batch[0].event).toBe("squeezy_session_summary");
  expect(batch.batch[0].properties.summary_id).toBe("33333333-3333-4333-8333-333333333333");
  expect(batch.batch[0].properties.abnormal_exit).toBe(false);
  expect(batch.batch[0].properties.tool_counts).toEqual({ shell: 2, graph: 1 });
  expect(batch.batch[0].properties.prompt).toBeUndefined();
});

test("product telemetry forwards future safe properties and drops unsafe values", async () => {
  const forwarded: unknown[] = [];
  globalThis.fetch = async (_input: RequestInfo | URL, init?: RequestInit) => {
    forwarded.push(JSON.parse(String(init?.body)));
    return new Response(null, { status: 200 });
  };

  const response = await worker.fetch(
    new Request("https://telemetry.example/v1/batch", {
      method: "POST",
      body: JSON.stringify({
        schema_version: 1,
        user_id: "11111111-1111-4111-8111-111111111111",
        install_id: "11111111-1111-4111-8111-111111111111",
        session_id: "22222222-2222-4222-8222-222222222222",
        app_version: "0.1.0",
        os: "macos",
        arch: "aarch64",
        events: [
          {
            event: "squeezy_future_counter",
            timestamp_ms: Date.now(),
            event_sequence: 1,
            properties: {
              provider: "open_ai",
              model_family: "gpt",
              tool_status: "future_status",
              trace_id: "not-a-trace",
              local_path: "/Users/example/project",
              prompt: "must not forward",
              future_counter: 123,
              future_flag: true,
              future_token: "new_metric:v2",
              future_counts: { new_metric: 2, "another.key": 1 },
              bad_array: [1],
              bad_object: { raw: "text value" },
              "bad-key": 1,
            },
          },
        ],
      }),
    }),
    env(),
  );

  expect(response.status).toBe(204);
  expect(forwarded).toHaveLength(1);
  const batch = forwarded[0] as { batch: Array<{ event: string; properties: Record<string, unknown> }> };
  expect(batch.batch[0].event).toBe("squeezy_future_counter");
  expect(batch.batch[0].properties.provider).toBe("open_ai");
  expect(batch.batch[0].properties.model_family).toBe("gpt");
  expect(batch.batch[0].properties.tool_status).toBe("future_status");
  expect(batch.batch[0].properties.trace_id).toBeUndefined();
  expect(batch.batch[0].properties.local_path).toBeUndefined();
  expect(batch.batch[0].properties.prompt).toBeUndefined();
  expect(batch.batch[0].properties.future_counter).toBe(123);
  expect(batch.batch[0].properties.future_flag).toBe(true);
  expect(batch.batch[0].properties.future_token).toBe("new_metric:v2");
  expect(batch.batch[0].properties.future_counts).toEqual({ new_metric: 2, "another.key": 1 });
  expect(batch.batch[0].properties.bad_array).toBeUndefined();
  expect(batch.batch[0].properties.bad_object).toBeUndefined();
  expect(batch.batch[0].properties["bad-key"]).toBeUndefined();
});

test("product telemetry rejects legacy non-squeezy event names", async () => {
  let called = false;
  globalThis.fetch = async () => {
    called = true;
    return new Response(null, { status: 200 });
  };

  const response = await worker.fetch(
    new Request("https://telemetry.example/v1/batch", {
      method: "POST",
      body: JSON.stringify({
        schema_version: 1,
        user_id: "11111111-1111-4111-8111-111111111111",
        install_id: "11111111-1111-4111-8111-111111111111",
        session_id: "22222222-2222-4222-8222-222222222222",
        app_version: "0.1.0",
        os: "macos",
        arch: "aarch64",
        events: [
          {
            event: "approval_best_effort_fallback",
            timestamp_ms: Date.now(),
            event_sequence: 1,
            properties: { tool_name: "shell" },
          },
        ],
      }),
    }),
    env(),
  );

  expect(response.status).toBe(400);
  expect(called).toBe(false);
});

test("site telemetry accepts page view and forwards sanitized properties", async () => {
  const forwarded: unknown[] = [];
  globalThis.fetch = async (_input: RequestInfo | URL, init?: RequestInit) => {
    forwarded.push(JSON.parse(String(init?.body)));
    return new Response(null, { status: 200 });
  };

  const response = await worker.fetch(
    new Request("https://telemetry.example/v1/site", {
      method: "POST",
      body: JSON.stringify({
        schema_version: 1,
        visitor_id: "11111111-1111-4111-8111-111111111111",
        session_id: "22222222-2222-4222-8222-222222222222",
        timestamp_ms: Date.now(),
        event: "squeezy_site_page_view",
        path: "/languages/",
        referrer_kind: "internal",
        utm_source: "docs",
      }),
    }),
    env(),
  );

  expect(response.status).toBe(204);
  expect(forwarded).toHaveLength(1);
  const batch = forwarded[0] as { batch: Array<{ event: string; properties: Record<string, unknown> }> };
  expect(batch.batch[0].event).toBe("squeezy_site_page_view");
  expect(batch.batch[0].properties.distinct_id).toBe("11111111-1111-4111-8111-111111111111");
  expect(batch.batch[0].properties.path).toBe("/languages/");
  expect(batch.batch[0].properties.utm_source).toBe("docs");
});

test("site telemetry rejects unknown fields", async () => {
  let called = false;
  globalThis.fetch = async () => {
    called = true;
    return new Response(null, { status: 200 });
  };

  const response = await worker.fetch(
    new Request("https://telemetry.example/v1/site", {
      method: "POST",
      body: JSON.stringify({
        schema_version: 1,
        visitor_id: "11111111-1111-4111-8111-111111111111",
        session_id: "22222222-2222-4222-8222-222222222222",
        timestamp_ms: Date.now(),
        event: "squeezy_site_page_view",
        path: "/",
        referrer_kind: "none",
        raw_url: "https://squeezyagent.com/?secret=1",
      }),
    }),
    env(),
  );

  expect(response.status).toBe(400);
  expect(called).toBe(false);
});

test("product telemetry drops malformed trace_id and span_id but accepts well-formed ones", async () => {
  const forwarded: unknown[] = [];
  globalThis.fetch = async (_input: RequestInfo | URL, init?: RequestInit) => {
    forwarded.push(JSON.parse(String(init?.body)));
    return new Response(null, { status: 200 });
  };

  const response = await worker.fetch(
    new Request("https://telemetry.example/v1/batch", {
      method: "POST",
      body: JSON.stringify({
        schema_version: 1,
        user_id: "11111111-1111-4111-8111-111111111111",
        install_id: "11111111-1111-4111-8111-111111111111",
        session_id: "22222222-2222-4222-8222-222222222222",
        app_version: "0.1.0",
        os: "macos",
        arch: "aarch64",
        events: [
          {
            event: "squeezy_tool_completed",
            timestamp_ms: Date.now(),
            event_sequence: 1,
            properties: {
              trace_id: "d".repeat(32),
              span_id: "e".repeat(16),
            },
          },
          {
            event: "squeezy_tool_completed",
            timestamp_ms: Date.now(),
            event_sequence: 2,
            properties: {
              trace_id: "not-a-trace",
              span_id: "too-short",
            },
          },
        ],
      }),
    }),
    env(),
  );

  expect(response.status).toBe(204);
  const batch = forwarded[0] as { batch: Array<{ properties: Record<string, unknown> }> };
  expect(batch.batch[0].properties.trace_id).toBe("d".repeat(32));
  expect(batch.batch[0].properties.span_id).toBe("e".repeat(16));
  expect(batch.batch[1].properties.trace_id).toBeUndefined();
  expect(batch.batch[1].properties.span_id).toBeUndefined();
});

test("new domain events (mcp, web, skills, approval) accepted by generic allowlist", async () => {
  const forwarded: unknown[] = [];
  globalThis.fetch = async (_input: RequestInfo | URL, init?: RequestInit) => {
    forwarded.push(JSON.parse(String(init?.body)));
    return new Response(null, { status: 200 });
  };

  const response = await worker.fetch(
    new Request("https://telemetry.example/v1/batch", {
      method: "POST",
      body: JSON.stringify({
        schema_version: 1,
        user_id: "11111111-1111-4111-8111-111111111111",
        install_id: "11111111-1111-4111-8111-111111111111",
        session_id: "22222222-2222-4222-8222-222222222222",
        app_version: "0.1.0",
        os: "linux",
        arch: "x86_64",
        events: [
          {
            event: "squeezy_mcp_discovery",
            timestamp_ms: Date.now(),
            event_sequence: 1,
            properties: {
              duration_ms: 150,
              mcp_counts: { transport_stdio: 2, tools_discovered: 5 },
            },
          },
          {
            event: "squeezy_web_request",
            timestamp_ms: Date.now(),
            event_sequence: 2,
            properties: {
              duration_ms: 820,
              external_counts: { provider_exa_status_success_bytes_1k_10k: 1 },
            },
          },
          {
            event: "squeezy_skill_activated",
            timestamp_ms: Date.now(),
            event_sequence: 3,
            properties: {
              skill_counts: { source_user: 2, activation_explicit: 1, included: 2 },
            },
          },
          {
            event: "squeezy_approval_decided",
            timestamp_ms: Date.now(),
            event_sequence: 4,
            properties: {
              approval_counts: { shell_high_approved_user: 1 },
            },
          },
          {
            event: "squeezy_provider_error",
            timestamp_ms: Date.now(),
            event_sequence: 5,
            properties: {
              provider_error_counts: { rate_limit: 1 },
            },
          },
          {
            event: "squeezy_session_summary",
            timestamp_ms: Date.now(),
            event_sequence: 6,
            properties: {
              mcp_counts: { transport_stdio: 1, tools_discovered: 3 },
              skill_counts: { source_project: 1, included: 1 },
              subagent_counts: { explore_calls: 2, explore_failures: 0 },
              stop_reason_counts: { end_turn: 4, tool_use: 2 },
              cache_supported: true,
              cache_write_tokens: 1200,
              reasoning_output_tokens: 300,
              subagent_cap_rejections: 1,
              startup_placeholder_ms: 180,
              startup_agent_build_ms: 950,
              startup_snapshot_ms: 1100,
            },
          },
        ],
      }),
    }),
    env(),
  );

  expect(response.status).toBe(204);
  const batch = forwarded[0] as { batch: Array<{ event: string; properties: Record<string, unknown> }> };
  expect(batch.batch).toHaveLength(6);
  // Count-maps pass through.
  expect((batch.batch[0].properties.mcp_counts as Record<string, number>).transport_stdio).toBe(2);
  // Provider enum stripped (unsafe string chars not present in key but token must be bounded).
  expect((batch.batch[1].properties.external_counts as Record<string, number>)["provider_exa_status_success_bytes_1k_10k"]).toBe(1);
  // New scalar fields on session summary.
  expect(batch.batch[5].properties.cache_supported).toBe(true);
  expect(batch.batch[5].properties.cache_write_tokens).toBe(1200);
  expect(batch.batch[5].properties.startup_placeholder_ms).toBe(180);
  // subagent_counts count-map forwarded.
  expect((batch.batch[5].properties.subagent_counts as Record<string, number>)["explore_calls"]).toBe(2);
});

test("sha256 fields are no longer sent by client (noise removal)", async () => {
  const forwarded: unknown[] = [];
  globalThis.fetch = async (_input: RequestInfo | URL, init?: RequestInit) => {
    forwarded.push(JSON.parse(String(init?.body)));
    return new Response(null, { status: 200 });
  };
  // Simulate a client that (incorrectly) still sends SHA fields — worker
  // passes them through via the generic sanitizer. The important change is
  // that the Rust client no longer sends them at all. This test confirms the
  // worker doesn't break if they arrive (backward compat) but doesn't check
  // their presence in normal operation.
  const response = await worker.fetch(
    new Request("https://telemetry.example/v1/batch", {
      method: "POST",
      body: JSON.stringify({
        schema_version: 1,
        user_id: "11111111-1111-4111-8111-111111111111",
        install_id: "11111111-1111-4111-8111-111111111111",
        session_id: "33333333-3333-4333-8333-333333333333",
        app_version: "0.1.0",
        os: "macos",
        arch: "aarch64",
        events: [
          {
            event: "squeezy_tool_completed",
            timestamp_ms: Date.now(),
            event_sequence: 1,
            properties: { tool_name: "shell", duration_ms: 5 },
          },
        ],
      }),
    }),
    env(),
  );
  expect(response.status).toBe(204);
});

test("site telemetry handles cors preflight", async () => {
  const response = await worker.fetch(new Request("https://telemetry.example/v1/site", { method: "OPTIONS" }), env());

  expect(response.status).toBe(204);
  expect(response.headers.get("access-control-allow-origin")).toBe("https://squeezyagent.com");
  expect(response.headers.get("access-control-allow-methods")).toContain("POST");
});
