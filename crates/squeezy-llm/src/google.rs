use std::collections::BTreeMap;
use std::sync::Arc;

use async_stream::try_stream;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use squeezy_core::{CostSnapshot, GoogleConfig, ProviderTransportConfig, Result, SqueezyError};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::{
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall, ReasoningKind,
    ReasoningPayload,
    credentials::{ApiKeySource, resolve_api_key_with_inline, static_api_key_source},
    retry::{RetryPolicy, idle_timeout, send_with_auth_retry, with_stream_retry},
    sse::SseDecoder,
    transport::shared_client,
};

#[derive(Clone)]
pub struct GoogleProvider {
    client: reqwest::Client,
    api_key: Arc<dyn ApiKeySource>,
    base_url: String,
    transport: ProviderTransportConfig,
}

impl std::fmt::Debug for GoogleProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleProvider")
            .field("client", &self.client)
            .field("api_key", &self.api_key)
            .field("base_url", &self.base_url)
            .field("transport", &self.transport)
            .finish()
    }
}

impl GoogleProvider {
    pub fn from_config(config: &GoogleConfig) -> Result<Self> {
        let api_key =
            resolve_api_key_with_inline(config.api_key.as_deref(), &config.api_key_env)?.value;
        let base_url = config.base_url.trim_end_matches('/').to_string();
        // Validate that the base URL points at a Gemini API version
        // path (`/v1`, `/v1beta`, `/v1alpha`, etc.). Pre-fix a base
        // URL of `https://example.com` silently produced
        // `https://example.com/models/...:streamGenerateContent` —
        // wrong URL with no warning. Bare hosts with no version path
        // are almost always a configuration error.
        validate_google_base_url(&base_url)?;
        Ok(Self {
            client: shared_client(&config.transport),
            api_key: static_api_key_source(api_key, "google"),
            base_url,
            transport: config.transport,
        })
    }

    pub(crate) fn request_body(request: &LlmRequest) -> Value {
        // Canonicalize tool-call ids and synthesize placeholders for
        // orphan tool results BEFORE projecting to Google's
        // `contents` array. Google identifies tool calls by `name`
        // (no explicit id) and pairs `functionResponse` to the
        // preceding `functionCall` by name; cross-provider replay
        // can leave `FunctionCallOutput` items whose `call_id`
        // doesn't appear in any prior `FunctionCall`, in which case
        // the response gets dropped to a generic `"tool"` name and
        // the model can't follow the conversation. Synthesizing a
        // placeholder call keeps the pairing intact.
        let normalized_input = crate::normalize_tool_ids_for_replay(&request.input);
        let mut body = json!({
            "systemInstruction": {
                "parts": [{"text": request.instructions}]
            },
            "contents": google_contents(&normalized_input),
            "generationConfig": {},
        });
        if let Some(max_output_tokens) = request.max_output_tokens {
            body["generationConfig"]["maxOutputTokens"] = json!(max_output_tokens);
        }
        // Gemini 2.5 thinks by default; the API just won't return thought
        // summaries unless `includeThoughts` is on. Mirror OpenAI: request
        // summaries whenever the model is reasoning-capable, and only set
        // an explicit `thinkingBudget` when the caller picked an effort.
        //
        // Caps signal is OR-of-three: either the legacy `reasoning_effort`
        // flag, the Phase 1 `default_reasoning_effort` (registers a
        // recommended baseline even when the wire field is fixed), or an
        // explicit per-request `reasoning_effort`. Without the
        // `default_reasoning_effort` arm, Gemini 2.5 models which carry
        // `reasoning_effort: false` in models.json never get
        // `includeThoughts` and the ReasoningDelta path is dead code even
        // though users are billed for `thoughtsTokenCount`.
        let caps = crate::capabilities_for("google", &request.model);
        let reasoning_capable = caps.is_some_and(|c| c.reasoning_effort)
            || caps.is_some_and(|c| c.default_reasoning_effort.is_some());
        if reasoning_capable || request.reasoning_effort.is_some() {
            let mut thinking = json!({ "includeThoughts": true });
            if let Some(effort) = request.reasoning_effort {
                // Clamp per-model. ReasoningEffort::thinking_budget_tokens
                // returns an Anthropic-shaped scale (XHigh = 60_000) that
                // exceeds Gemini 2.5's documented maxima (Pro 32_768,
                // Flash / Flash-Lite 24_576). Pre-clamp, every
                // `xhigh` caller on Gemini 2.5 got a 400. The Phase 3
                // registry will carry per-model
                // `thinking_budget_min` / `thinking_budget_max` so the
                // clamp tightens to the right ranges automatically.
                let raw = effort.thinking_budget_tokens();
                let clamped = clamp_thinking_budget(caps.as_ref(), raw);
                thinking["thinkingBudget"] = json!(clamped);
            }
            body["generationConfig"]["thinkingConfig"] = thinking;
        }
        if !request.tools.is_empty() {
            let mut function_declarations = Vec::with_capacity(request.tools.len());
            for tool in request.tools.iter() {
                function_declarations.push(json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": sanitize_for_gemini(&tool.parameters),
                }));
            }
            body["tools"] = json!([{
                "functionDeclarations": function_declarations
            }]);
            // Forward `tool_choice` as `toolConfig.functionCallingConfig.mode`.
            // Without this, tool-shy small models (Qwen via aggregators et al.)
            // ignore the tools list because Gemini's default mode is AUTO.
            // Map auto/none → AUTO/NONE, required → ANY (Gemini's "must call
            // some tool" mode). Unrecognized values fall through to Gemini's
            // default. opencode gemini.ts:173-179 is the reference.
            if let Some(mode) = tool_choice_to_gemini_mode(request.tool_choice.as_deref()) {
                body["toolConfig"] = json!({
                    "functionCallingConfig": { "mode": mode }
                });
            }
        }
        // Structured-output: when callers ship an output_schema, ask
        // Gemini to emit JSON and pin the schema via responseMimeType
        // + responseSchema. Sanitize through the same Gemini pass so
        // $ref / additionalProperties / nullable-union shapes don't
        // trip the server-side validator. Reference:
        // https://ai.google.dev/gemini-api/docs/structured-output.
        if let Some(schema) = request.output_schema.as_ref() {
            body["generationConfig"]["responseMimeType"] = json!("application/json");
            body["generationConfig"]["responseSchema"] = sanitize_for_gemini(&schema.schema);
        }
        body
    }
}

fn tool_choice_to_gemini_mode(choice: Option<&str>) -> Option<&'static str> {
    match choice? {
        "auto" => Some("AUTO"),
        "none" => Some("NONE"),
        "required" => Some("ANY"),
        _ => None,
    }
}

/// Gemini's documented inline image MIME list. Anything outside this
/// set returns a 400. Reference:
/// https://ai.google.dev/gemini-api/docs/image-understanding.
fn is_supported_gemini_image_mime(media_type: &str) -> bool {
    matches!(
        media_type,
        "image/png" | "image/jpeg" | "image/webp" | "image/heic" | "image/heif"
    )
}

/// Validate that the Google base URL ends in a `/vN[suffix]` API
/// version segment. The Gemini wire shape is
/// `{base}/models/{model}:streamGenerateContent`; bare hosts produce
/// a syntactically valid but semantically wrong URL.
pub(crate) fn validate_google_base_url(base_url: &str) -> Result<()> {
    let segment = base_url.rsplit('/').next().unwrap_or("");
    let is_versioned = segment.starts_with('v')
        && segment.len() >= 2
        && segment[1..2].chars().all(|c| c.is_ascii_digit());
    if !is_versioned {
        return Err(SqueezyError::ProviderRequest(format!(
            "Google base_url `{base_url}` is missing a /v* API version \
             segment (expected `…/v1`, `…/v1beta`, `…/v1alpha`)."
        )));
    }
    Ok(())
}

/// Gemini's `:streamGenerateContent` endpoint rejects requests whose
/// total body exceeds 20 MB. Each `Image` becomes a base64
/// `inlineData` part (~33% larger than raw bytes), so 15 MB of raw
/// image already saturates the cap. Pre-check inline image sizes so
/// callers see a structured error pointing at the File API rather
/// than a vendor 400 INVALID_ARGUMENT.
const GEMINI_INLINE_IMAGE_LIMIT_BYTES: usize = 20 * 1024 * 1024;

fn check_inline_image_cap(request: &LlmRequest) -> Result<()> {
    let mut encoded_total: usize = 0;
    for item in request.input.iter() {
        if let LlmInputItem::Image { bytes, .. } = item {
            // base64 expands 3 bytes → 4 chars.
            encoded_total = encoded_total.saturating_add(bytes.len().div_ceil(3).saturating_mul(4));
            if encoded_total > GEMINI_INLINE_IMAGE_LIMIT_BYTES {
                return Err(SqueezyError::ProviderRequest(format!(
                    "Google inline image payload exceeds Gemini's 20 MB limit \
                     (~{} MB encoded). Use Google's File API for larger \
                     uploads.",
                    encoded_total / (1024 * 1024)
                )));
            }
        }
    }
    Ok(())
}

/// Clamp a raw `thinking_budget_tokens` value (from `ReasoningEffort`)
/// to the per-model max / min that Phase 3 stamps into the registry.
/// `caps == None` (off-registry model) or `caps` without the new fields
/// leaves the raw value in place to preserve historical behavior.
pub(crate) fn clamp_thinking_budget(caps: Option<&crate::ModelCapabilities>, raw: u32) -> u32 {
    let mut value = raw;
    if let Some(caps) = caps {
        if let Some(max) = caps.thinking_budget_max {
            value = value.min(max);
        }
        if let Some(min) = caps.thinking_budget_min {
            value = value.max(min);
        }
    }
    value
}

/// Project a JSON Schema into Gemini's OpenAPI-3.03 subset. Gemini's
/// `functionDeclarations[].parameters` rejects: `additionalProperties`,
/// `$ref` / `$defs`, empty `{"type":"object"}` (must have
/// `properties`), and `type: [..., "null"]` (uses `nullable: true`
/// instead). Without this pass, schemas that work on Anthropic /
/// OpenAI return 400 INVALID_ARGUMENT on Gemini.
///
/// Mirrors opencode's gemini sanitize pipeline (others/opencode/packages/llm/src/protocols/gemini.ts).
pub(crate) fn sanitize_for_gemini(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map.iter() {
                // Drop keys Gemini rejects outright. `$defs` / `$ref`
                // can't be resolved server-side; `additionalProperties`
                // and `unevaluatedProperties` aren't in the supported
                // subset.
                match key.as_str() {
                    "$ref"
                    | "$defs"
                    | "$schema"
                    | "$id"
                    | "$comment"
                    | "additionalProperties"
                    | "unevaluatedProperties" => continue,
                    _ => {}
                }
                let sanitized = match key.as_str() {
                    "properties" => {
                        if let Some(obj) = value.as_object() {
                            let mut new_obj = serde_json::Map::new();
                            for (k, v) in obj.iter() {
                                new_obj.insert(k.clone(), sanitize_for_gemini(v));
                            }
                            Value::Object(new_obj)
                        } else {
                            value.clone()
                        }
                    }
                    "items" | "allOf" | "anyOf" | "oneOf" | "prefixItems" => match value {
                        Value::Array(arr) => {
                            Value::Array(arr.iter().map(sanitize_for_gemini).collect())
                        }
                        other => sanitize_for_gemini(other),
                    },
                    "type" => {
                        if let Value::Array(arr) = value {
                            // `["string", "null"]` → keep `"string"` and
                            // set `nullable: true` (Gemini's idiom).
                            let mut nullable = false;
                            let mut other: Option<Value> = None;
                            for elem in arr {
                                if elem == "null" {
                                    nullable = true;
                                } else if other.is_none() {
                                    other = Some(elem.clone());
                                }
                            }
                            if nullable {
                                out.insert("nullable".to_string(), Value::Bool(true));
                            }
                            if let Some(other) = other {
                                other
                            } else {
                                value.clone()
                            }
                        } else {
                            value.clone()
                        }
                    }
                    _ => value.clone(),
                };
                out.insert(key.clone(), sanitized);
            }
            // Empty `{"type":"object"}` (no `properties`) is rejected
            // by Gemini with "should be non-empty for OBJECT type".
            // Synthesize an empty `properties` map so the wire shape
            // stays valid.
            if matches!(out.get("type"), Some(Value::String(t)) if t == "object")
                && !out.contains_key("properties")
            {
                out.insert(
                    "properties".to_string(),
                    Value::Object(serde_json::Map::new()),
                );
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sanitize_for_gemini).collect()),
        other => other.clone(),
    }
}

impl LlmProvider for GoogleProvider {
    fn name(&self) -> &'static str {
        "google"
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
        if let Err(err) = request.ensure_vision_support("google") {
            return Box::pin(futures_util::stream::once(async move { Err(err) }));
        }
        if let Err(err) = request.reject_unsupported_documents("google") {
            return Box::pin(futures_util::stream::once(async move { Err(err) }));
        }
        if let Err(err) = check_inline_image_cap(&request) {
            return Box::pin(futures_util::stream::once(async move { Err(err) }));
        }
        let client = self.client.clone();
        // Keep the API key off the URL: `reqwest::Error::Display` appends
        // `" for url ({url})"` to every transport/stream error message, so a
        // key-in-query URL would leak the key into `SqueezyError::ProviderRequest`
        // / `ProviderStream`, the CLI/TUI status line, logs, tracing, and bug
        // reports on any DNS/TLS/timeout/connection or chunk error. Send it via
        // Google's documented `x-goog-api-key` header instead.
        let url = google_stream_url(&self.base_url, &request.model);
        let api_key = self.api_key.clone();
        let transport = self.transport;
        let request_for_attempts = request.clone();
        let attempt_cancel = cancel.clone();
        let make_attempt = move || -> LlmStream {
            google_stream_attempt(
                client.clone(),
                api_key.clone(),
                url.clone(),
                request_for_attempts.clone(),
                transport,
                attempt_cancel.clone(),
            )
        };
        // Mirror Anthropic — wrap in with_stream_retry so transient
        // mid-stream truncation (RST, idle timeout on a long thinking
        // turn, partial frame) triggers a bounded reconnect that
        // dedupes already-yielded events via StreamSkipState instead
        // of losing the entire turn.
        with_stream_retry(
            "google",
            RetryPolicy::provider_stream(transport),
            cancel,
            make_attempt,
        )
    }
}

fn google_stream_attempt(
    client: reqwest::Client,
    api_key: Arc<dyn ApiKeySource>,
    url: String,
    request: LlmRequest,
    transport: ProviderTransportConfig,
    cancel: CancellationToken,
) -> LlmStream {
    let body = GoogleProvider::request_body(&request);
    Box::pin(try_stream! {
        let response = send_with_auth_retry(
            &api_key,
            RetryPolicy::provider_requests(transport),
            &cancel,
            |key| {
                client
                    .post(&url)
                    .header("x-goog-api-key", key)
                    .json(&body)
            },
        ).await?;
        let status = response.status();
        let response = if status == StatusCode::OK {
            response
        } else {
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "failed to read error response".to_string());
            Err(SqueezyError::ProviderRequest(format!("{status}: {message}")))?;
            unreachable!("provider error returned above");
        };

        // Defer Started until the first successful parse_google_event.
        // Pre-fix Started fired right after HTTP 200, so first-chunk
        // parse failures, empty 200 bodies, and pre-stream errors
        // surfaced as "Started but no Completed" dangling state.
        let mut started_emitted = false;
        let mut decoder = SseDecoder::default();
        let mut last_cost = CostSnapshot::default();
        let mut last_finish_reason: Option<String> = None;
        let mut server_model_slot: Option<String> = None;
        let mut server_model_echo = crate::ServerModelEcho::default();
        let mut saw_any = false;
        let mut reasoning_buf = GoogleReasoningBuffer::default();
        // Per-stream tool-call counter. Gemini doesn't issue tool-call
        // ids on the wire — we synthesize one. Two streamed events
        // each carrying `functionCall` at `parts[0]` previously
        // collided on `google_call_0` because the counter was the
        // part index within a *single* SSE event. The replay
        // canonicalizer then collapsed both calls and the second
        // FunctionCallOutput overrode the first. Lift the counter
        // to a per-stream usize so parallel calls keep distinct ids.
        let mut tool_call_counter: usize = 0;
        let mut response_id_slot: Option<String> = None;
        let mut bytes = response.bytes_stream();
        loop {
            let polled = tokio::select! {
                _ = cancel.cancelled() => {
                    yield LlmEvent::Cancelled;
                    return;
                }
                next = timeout(idle_timeout(transport), bytes.next()) => next,
            };
            let next = polled.map_err(|_| {
                SqueezyError::ProviderStream("Google stream idle timeout".to_string())
            })?;
            let Some(chunk) = next else { break; };
            let chunk = chunk.map_err(|err| SqueezyError::ProviderStream(err.to_string()))?;
            for event in decoder.push(&chunk) {
                saw_any = true;
                let parsed = parse_google_event(
                    &event,
                    &mut last_cost,
                    &mut last_finish_reason,
                    &mut reasoning_buf,
                    &mut server_model_slot,
                    &mut tool_call_counter,
                    &mut response_id_slot,
                )?;
                if !started_emitted {
                    started_emitted = true;
                    yield LlmEvent::Started;
                }
                if let Some(server) = server_model_slot.take()
                    && let Some(echo) = server_model_echo.observe(&request.model, &server)
                {
                    yield echo;
                }
                for llm_event in parsed {
                    yield llm_event;
                }
            }
        }
        for event in decoder.finish() {
            saw_any = true;
            let parsed = parse_google_event(
                &event,
                &mut last_cost,
                &mut last_finish_reason,
                &mut reasoning_buf,
                &mut server_model_slot,
                &mut tool_call_counter,
                &mut response_id_slot,
            )?;
            if !started_emitted {
                started_emitted = true;
                yield LlmEvent::Started;
            }
            if let Some(server) = server_model_slot.take()
                && let Some(echo) = server_model_echo.observe(&request.model, &server)
            {
                yield echo;
            }
            for llm_event in parsed {
                yield llm_event;
            }
        }
        if !saw_any {
            Err(SqueezyError::ProviderStream("Google stream ended without events".to_string()))?;
        }
        // Defensive: if every parsed event returned Ok(vec![]) (no
        // candidates, no usage, no parts) the Started latch never
        // flipped. Emit it now so consumers always see Started before
        // Completed.
        if !started_emitted {
            yield LlmEvent::Started;
        }
        if let Some(payload) = reasoning_buf.flush() {
            yield LlmEvent::ReasoningDone(payload);
        }
        yield LlmEvent::Completed {
            response_id: response_id_slot,
            cost: last_cost,
            stop_reason: last_finish_reason
                .as_deref()
                .map(crate::StopReason::from_google),
            reasoning_only_stop: false,
        };
    })
}

pub(crate) fn google_stream_url(base_url: &str, model: &str) -> String {
    format!("{base_url}/models/{model}:streamGenerateContent?alt=sse")
}

fn google_contents(input: &[LlmInputItem]) -> Value {
    let mut contents = Vec::with_capacity(input.len());
    let mut tool_names_by_call_id = BTreeMap::new();
    for item in input {
        match item {
            LlmInputItem::UserText(text) => contents.push(json!({
                "role": "user",
                "parts": [{"text": text}],
            })),
            LlmInputItem::AssistantText(text) => contents.push(json!({
                "role": "model",
                "parts": [{"text": text}],
            })),
            LlmInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                tool_names_by_call_id.insert(call_id.as_str(), name.as_str());
                contents.push(json!({
                    "role": "model",
                    "parts": [{"functionCall": {"name": name, "args": arguments}}],
                }));
            }
            LlmInputItem::FunctionCallOutput {
                call_id,
                output,
                is_error,
                ..
            } => {
                let name = tool_names_by_call_id
                    .get(call_id.as_str())
                    .copied()
                    .unwrap_or("tool");
                // Gemini's `functionResponse.response` switches its
                // shape on success vs failure: `{output: str}` on
                // success, `{error: str}` on failure. Treating every
                // result as success made the model re-call after
                // errors. Use the Phase 1 `is_error` flag to pick the
                // right key. Reference: opencode / pi google-shared
                // protocol.
                let key = if *is_error { "error" } else { "output" };
                contents.push(json!({
                    "role": "function",
                    "parts": [{"functionResponse": {
                        "name": name,
                        "response": {key: output},
                    }}],
                }));
            }
            LlmInputItem::Image { media_type, bytes } => {
                // NIT — validate against Gemini's documented inline
                // image MIME list. Caller-supplied media_type is
                // shipped to the wire as-is; if it doesn't match a
                // supported type the upstream returns a 400. Replace
                // a clearly-wrong MIME with one inferred from the
                // bytes when possible (falls back to caller's value
                // for sniffer misses so we don't regress shipping
                // unrecognized-but-valid types).
                let effective_mime: &str = if is_supported_gemini_image_mime(media_type) {
                    media_type
                } else if let Some(inferred) = crate::infer_image_mime(bytes.as_ref()) {
                    inferred
                } else {
                    media_type
                };
                contents.push(json!({
                    "role": "user",
                    "parts": [{
                        "inlineData": {
                            "mimeType": effective_mime,
                            "data": BASE64_STANDARD.encode(bytes.as_ref()),
                        },
                    }],
                }));
            }
            LlmInputItem::Reasoning(ReasoningPayload::Google {
                summary,
                thought_signature,
            }) => {
                let mut parts = Vec::with_capacity(summary.len());
                for text in summary {
                    let mut part = json!({
                        "text": text,
                        "thought": true,
                    });
                    if let Some(sig) = thought_signature {
                        part["thoughtSignature"] = json!(sig);
                    }
                    parts.push(part);
                }
                if !parts.is_empty() {
                    contents.push(json!({
                        "role": "model",
                        "parts": parts,
                    }));
                }
            }
            // Reasoning items from other providers are dropped when replaying to Google.
            LlmInputItem::Reasoning(_) => {}
            // Google Gemini accepts document inlineData (pdf etc.).
            // Per-provider lowering lands in Phase 4; for now we skip
            // with a debug log.
            LlmInputItem::Document { name, .. } => {
                tracing::debug!(
                    target: "squeezy_llm::google",
                    name = name.as_str(),
                    "google document content block not yet implemented; skipping",
                );
            }
        }
    }
    Value::Array(contents)
}

/// TODO (M-08 / M-09): per-part `thoughtSignature` preservation. Today
/// `ReasoningPayload::Google` carries `summary: Vec<String>` + a single
/// `Option<String>` signature, so on replay every summary part gets
/// the same signature and signatures on text / functionCall parts are
/// dropped entirely. This works for Gemini 2.5 by coincidence but
/// breaks Gemini 3 multi-turn tool use (pi #1829).
///
/// The full fix needs `ReasoningPayload::Google` to carry
/// `Vec<(text, Option<sig>)>` and the lib-side `LlmInputItem::AssistantText`
/// / `FunctionCall` variants to grow optional `thoughtSignature` fields.
/// Both live in lib.rs / squeezy-core (outside this provider's
/// ownership window). The buffer below preserves the *last* signature
/// observed within a chunk; that's the de facto Gemini 2.5 shape pi
/// historically shipped and unblocks the Phase 4C migration without a
/// schema change.
#[derive(Debug, Default)]
struct GoogleReasoningBuffer {
    summary: Vec<String>,
    signature: Option<String>,
}

impl GoogleReasoningBuffer {
    fn push(&mut self, text: &str, signature: Option<&str>) {
        if !text.is_empty() {
            self.summary.push(text.to_string());
        }
        if let Some(sig) = signature
            && self.signature.as_deref() != Some(sig)
        {
            self.signature = Some(sig.to_string());
        }
    }

    fn flush(&mut self) -> Option<ReasoningPayload> {
        if self.summary.is_empty() && self.signature.is_none() {
            return None;
        }
        let summary = std::mem::take(&mut self.summary);
        let thought_signature = self.signature.take();
        Some(ReasoningPayload::Google {
            summary,
            thought_signature,
        })
    }
}

fn parse_google_event(
    data: &str,
    cost: &mut CostSnapshot,
    last_finish_reason: &mut Option<String>,
    reasoning_buf: &mut GoogleReasoningBuffer,
    server_model_slot: &mut Option<String>,
    tool_call_counter: &mut usize,
    response_id_slot: &mut Option<String>,
) -> Result<Vec<LlmEvent>> {
    let value: Value = serde_json::from_str(data)
        .map_err(|err| SqueezyError::ProviderStream(format!("invalid Google SSE JSON: {err}")))?;
    if let Some(error) = value.get("error") {
        // Google's standard error envelope is
        // `{error:{code,message,status,details}}`. The previous code
        // only surfaced `message`, dropping the `status` /`code` tokens
        // that name the failure (e.g. "RESOURCE_EXHAUSTED"). Concatenate
        // status + code + message so the error text the caller sees is
        // self-describing. Reference:
        // https://cloud.google.com/apis/design/errors
        //
        // Note: this enrichment is purely cosmetic for retry decisions on
        // the stream path. `retry.rs::is_terminal_quota_error` only runs on
        // the *non-OK HTTP body* path, and its substring list does not even
        // include "RESOURCE_EXHAUSTED" — it never sees these in-stream SSE
        // error frames. Stream-path terminality is decided here instead, by
        // stamping the NON_RETRYABLE_MARKER onto the envelope below.
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Google stream error");
        let status = error.get("status").and_then(Value::as_str);
        let code = error.get("code").and_then(Value::as_i64);
        // Deterministic provider-side rejections fail identically on resend;
        // stamp the non-retryable marker so the stream-retry harness does not
        // burn its whole reconnect budget re-sending an identical request.
        let terminal = matches!(
            status,
            Some(
                "RESOURCE_EXHAUSTED"
                    | "INVALID_ARGUMENT"
                    | "PERMISSION_DENIED"
                    | "FAILED_PRECONDITION"
                    | "UNAUTHENTICATED"
                    | "NOT_FOUND"
            )
        );
        let mut text = String::new();
        if terminal {
            text.push_str(crate::anthropic_error::NON_RETRYABLE_MARKER);
        }
        if let Some(status) = status {
            text.push_str(status);
            text.push_str(": ");
        }
        if let Some(code) = code {
            text.push_str(&format!("[{code}] "));
        }
        text.push_str(message);
        return Err(SqueezyError::ProviderStream(text));
    }
    // Safety / content-policy blocks on the *prompt* arrive as an SSE
    // event with no candidates, only `promptFeedback.blockReason`.
    // Without this branch, `Ok(vec![])` returns and the outer loop
    // closes with `Completed { stop_reason: None }` — the agent sees
    // a silent zero-output success and retries forever. Surface the
    // block reason as a hard error so callers can show a real
    // message. Docs: https://ai.google.dev/api/generate-content
    if let Some(block_reason) = value
        .get("promptFeedback")
        .and_then(|fb| fb.get("blockReason"))
        .and_then(Value::as_str)
    {
        // A prompt-level content-policy block is deterministic: the same
        // prompt will be blocked on every resend, so mark it non-retryable.
        return Err(SqueezyError::ProviderStream(format!(
            "{}Google blocked prompt: {block_reason}",
            crate::anthropic_error::NON_RETRYABLE_MARKER
        )));
    }
    if server_model_slot.is_none()
        && let Some(server_model) = value.get("modelVersion").and_then(Value::as_str)
        && !server_model.is_empty()
    {
        // Google's `streamGenerateContent` echoes `modelVersion` on
        // every chunk (the pinned snapshot id, e.g. `gemini-2.5-pro` →
        // `gemini-2.5-pro-002`). Capture the first occurrence; the
        // outer stream loop drains the slot and emits `ServerModel`
        // once when the snapshot id differs from `request.model`.
        *server_model_slot = Some(server_model.to_string());
    }
    // Gemini's GenerateContentResponse carries `responseId` — the
    // natural correlation id for tracing / replay. Pre-fix it was
    // ignored and Completed.response_id was hard-coded to None.
    if response_id_slot.is_none()
        && let Some(rid) = value.get("responseId").and_then(Value::as_str)
        && !rid.is_empty()
    {
        *response_id_slot = Some(rid.to_string());
    }
    if let Some(usage) = value.get("usageMetadata") {
        // Token convention (X-11 / M-12). Gemini splits output across
        // two fields:
        // - `candidatesTokenCount` — *visible* output tokens (the
        //   model's text + tool-call args).
        // - `thoughtsTokenCount` — reasoning tokens consumed for
        //   the thinking pass (separate column on the bill).
        //
        // Gemini keeps the two disjoint in `usageMetadata`, but the
        // cross-provider `CostSnapshot` convention is the inclusive one:
        // `output_tokens` is the total billed output and
        // `reasoning_output_tokens` is the thoughts *subset* of it (see
        // the fold below and `token_split_pins_visible_vs_thoughts`).
        cost.input_tokens = usage.get("promptTokenCount").and_then(Value::as_u64);
        cost.cached_input_tokens = usage.get("cachedContentTokenCount").and_then(Value::as_u64);
        // Gemini's `usageMetadata` keeps thinking tokens disjoint:
        // `totalTokenCount = promptTokenCount + candidatesTokenCount
        // + thoughtsTokenCount`. The cross-provider `CostSnapshot`
        // convention (docs/internal/cost-saving/10-token-accounting.md)
        // requires `output_tokens` to include reasoning and
        // `reasoning_output_tokens` to be the subset breakdown, so fold
        // `thoughtsTokenCount` into the billed output. Without this,
        // `estimate_cost` prices thinking at $0.
        let visible = usage.get("candidatesTokenCount").and_then(Value::as_u64);
        let thoughts = usage.get("thoughtsTokenCount").and_then(Value::as_u64);
        cost.output_tokens = match (visible, thoughts) {
            (None, None) => None,
            (visible, thoughts) => Some(visible.unwrap_or(0) + thoughts.unwrap_or(0)),
        };
        cost.reasoning_output_tokens = thoughts;
    }
    if let Some(reason) = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.get("finishReason"))
        .and_then(Value::as_str)
        && last_finish_reason.as_deref() != Some(reason)
    {
        *last_finish_reason = Some(reason.to_string());
    }
    let parts = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array);
    let Some(parts) = parts else {
        return Ok(Vec::new());
    };
    let mut events = Vec::with_capacity(parts.len());
    for part in parts.iter() {
        let is_thought = part
            .get("thought")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(text) = part.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            if is_thought {
                let signature = part.get("thoughtSignature").and_then(Value::as_str);
                reasoning_buf.push(text, signature);
                events.push(LlmEvent::ReasoningDelta {
                    text: text.to_string(),
                    kind: ReasoningKind::Summary,
                });
                continue;
            }
            if let Some(payload) = reasoning_buf.flush() {
                events.push(LlmEvent::ReasoningDone(payload));
            }
            events.push(LlmEvent::TextDelta(text.to_string()));
        }
        if let Some(function_call) = part.get("functionCall") {
            if let Some(payload) = reasoning_buf.flush() {
                events.push(LlmEvent::ReasoningDone(payload));
            }
            let name = function_call
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    SqueezyError::ProviderStream("Google functionCall missing name".to_string())
                })?
                .to_string();
            let arguments = function_call
                .get("args")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            // Per-stream counter: parallel functionCall parts spread across
            // SSE events previously collided on `google_call_0` because the
            // index here was the part index within one event.
            let id = *tool_call_counter;
            *tool_call_counter += 1;
            events.push(LlmEvent::ToolCall(LlmToolCall {
                call_id: format!("google_call_{id}"),
                name,
                arguments,
            }));
        }
    }
    Ok(events)
}

#[cfg(test)]
#[path = "google_tests.rs"]
mod tests;
