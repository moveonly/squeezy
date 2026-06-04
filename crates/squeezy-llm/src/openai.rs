use std::collections::BTreeMap;
use std::sync::Arc;

use async_stream::try_stream;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use squeezy_core::{
    AzureOpenAiConfig, CostSnapshot, OpenAiCompatibleConfig, OpenAiCompatiblePreset, OpenAiConfig,
    ProviderTransportConfig, ResponseVerbosity, Result, SqueezyError,
};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::{
    INVALID_TOOL_ARGUMENTS_ERROR_KEY, INVALID_TOOL_ARGUMENTS_KEY, INVALID_TOOL_ARGUMENTS_RAW_KEY,
    LlmEvent, LlmInputItem, LlmOutputSchema, LlmProvider, LlmRequest, LlmStream, LlmToolCall,
    ReasoningKind, ReasoningPayload,
    credentials::{ApiKeySource, resolve_api_key_with_inline, static_api_key_source},
    openai_prompt_cache::clamp_prompt_cache_key,
    retry::{RetryPolicy, idle_timeout, send_with_auth_retry},
    sse::SseDecoder,
    transport::shared_client,
};

#[derive(Clone)]
pub struct OpenAiProvider {
    name: &'static str,
    client: reqwest::Client,
    api_key: Arc<dyn ApiKeySource>,
    base_url: String,
    api_version: Option<String>,
    auth_mode: OpenAiAuthMode,
    extra_headers: BTreeMap<String, String>,
    organization: Option<String>,
    project: Option<String>,
    service_tier: Option<String>,
    /// Logical model id → Azure-deployment name. Populated only from
    /// [`AzureOpenAiConfig::deployment_name_map`]; every other constructor
    /// leaves this empty so the model id passes through verbatim.
    deployment_name_map: BTreeMap<String, String>,
    transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAiAuthMode {
    Bearer,
    ApiKey,
    HeadersOnly,
}

impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("name", &self.name)
            .field("client", &self.client)
            .field("api_key", &self.api_key)
            .field("base_url", &self.base_url)
            .field("api_version", &self.api_version)
            .field("auth_mode", &self.auth_mode)
            .field(
                "extra_headers",
                &format_args!("<{} headers>", self.extra_headers.len()),
            )
            .field("organization", &self.organization)
            .field("project", &self.project)
            .field("service_tier", &self.service_tier)
            .field("deployment_name_map", &self.deployment_name_map)
            .field("transport", &self.transport)
            .finish()
    }
}

impl OpenAiProvider {
    pub fn from_config(config: &OpenAiConfig) -> Result<Self> {
        let api_key =
            resolve_api_key_with_inline(config.api_key.as_deref(), &config.api_key_env)?.value;
        let mut provider = Self::with_api_key_source(
            "openai",
            static_api_key_source(api_key, "openai"),
            config.base_url.trim_end_matches('/').to_string(),
            None,
            config.transport,
        );
        provider.organization = config.organization.clone();
        provider.project = config.project.clone();
        provider.service_tier = config.service_tier.clone();
        Ok(provider)
    }

    pub fn from_azure_config(config: &AzureOpenAiConfig) -> Result<Self> {
        if config.base_url.trim().is_empty() {
            return Err(SqueezyError::ProviderNotConfigured(
                "missing AZURE_OPENAI_BASE_URL or providers.azure_openai.base_url".to_string(),
            ));
        }
        // C-13: this provider concatenates `?api-version={config.api_version}`
        // onto every `/responses` URL. DEFAULT_AZURE_OPENAI_API_VERSION now
        // defaults to `"preview"` (the only api-version Azure serves the
        // Responses endpoint under), so a bare `AZURE_OPENAI_*` config works
        // out of the box. Operators can still pin a dated version via
        // `[providers.azure_openai].api_version`.
        let (api_key, auth_mode) = if config.use_entra_id {
            let token = config.entra_bearer_token.clone().ok_or_else(|| {
                SqueezyError::ProviderNotConfigured(
                    "missing AZURE_OPENAI_BEARER_TOKEN for Azure OpenAI Entra ID auth".to_string(),
                )
            })?;
            (token, OpenAiAuthMode::Bearer)
        } else if azure_headers_provide_auth(&config.extra_headers) {
            (String::new(), OpenAiAuthMode::HeadersOnly)
        } else {
            (
                resolve_api_key_with_inline(config.api_key.as_deref(), &config.api_key_env)?.value,
                OpenAiAuthMode::ApiKey,
            )
        };
        // H-36: detect the classic `/openai/deployments/{deployment}` URL
        // shape that older Azure Government / Mooncake resources still
        // use. The Responses route on these resources expects the URL to
        // already embed the deployment id, so the standard `{base}/responses`
        // rewrite (with the v1 `/openai/v1` path) breaks them. Stash a
        // flag here so `stream_response` can skip the deployment-name
        // rewrite below.
        let mut provider = Self::with_api_key_source_and_options(
            "azure_openai",
            static_api_key_source(api_key, "azure_openai"),
            config.base_url.trim_end_matches('/').to_string(),
            Some(config.api_version.clone()),
            auth_mode,
            config.extra_headers.clone(),
            None,
            None,
            None,
            config.transport,
        );
        provider.deployment_name_map = config.deployment_name_map.clone();
        Ok(provider)
    }

    /// `true` when this Azure provider was configured against the
    /// classic `/openai/deployments/{deployment}` URL shape. The
    /// Responses route on those resources expects the deployment to be
    /// in the URL path, not the request body, so [`stream_response`]
    /// skips the body-side model rewrite (and warns when neither the
    /// deployment nor the body model match).
    pub(crate) fn is_classic_azure_deployment_url(&self) -> bool {
        self.api_version.is_some() && self.base_url.contains("/openai/deployments/")
    }

    /// Build an OpenAI Responses-API client targeting xAI's `/responses`
    /// endpoint. Reuses the OpenAI request body and SSE parser because xAI
    /// implements the Responses wire as a near-drop-in for Grok 3 and Grok 4
    /// (see `https://docs.x.ai/docs/api-reference/responses`). The
    /// `OpenAiCompatibleConfig::extra_headers` map is forwarded so proxy,
    /// routing, telemetry, and attribution headers behave the same on xAI's
    /// Responses and Chat routes.
    pub fn from_xai_config(config: &OpenAiCompatibleConfig) -> Result<Self> {
        debug_assert_eq!(config.preset, OpenAiCompatiblePreset::XAi);
        if config.base_url.trim().is_empty() {
            return Err(SqueezyError::ProviderNotConfigured(
                "providers.xai.base_url is required for the xAI Responses route".to_string(),
            ));
        }
        let api_key =
            resolve_api_key_with_inline(config.api_key.as_deref(), &config.api_key_env)?.value;
        Ok(Self::with_api_key_source(
            "xai",
            static_api_key_source(api_key, "xai"),
            config.base_url.trim_end_matches('/').to_string(),
            None,
            config.transport,
        )
        .with_extra_headers(config.extra_headers.clone()))
    }

    /// Construct the provider against an already-built credential
    /// source. Used by the OpenAI Codex (ChatGPT Plus/Pro) OAuth
    /// provider so a rotating access token can flow through the same
    /// `/responses` HTTP path without rebuilding the client.
    pub fn with_api_key_source(
        name: &'static str,
        api_key: Arc<dyn ApiKeySource>,
        base_url: String,
        api_version: Option<String>,
        transport: ProviderTransportConfig,
    ) -> Self {
        Self::with_api_key_source_and_options(
            name,
            api_key,
            base_url,
            api_version,
            OpenAiAuthMode::Bearer,
            BTreeMap::new(),
            None,
            None,
            None,
            transport,
        )
    }

    pub(crate) fn with_extra_headers(mut self, extra_headers: BTreeMap<String, String>) -> Self {
        self.extra_headers = extra_headers;
        self
    }

    fn with_api_key_source_and_options(
        name: &'static str,
        api_key: Arc<dyn ApiKeySource>,
        base_url: String,
        api_version: Option<String>,
        auth_mode: OpenAiAuthMode,
        extra_headers: BTreeMap<String, String>,
        organization: Option<String>,
        project: Option<String>,
        service_tier: Option<String>,
        transport: ProviderTransportConfig,
    ) -> Self {
        Self {
            name,
            client: shared_client(&transport),
            api_key,
            base_url,
            api_version,
            auth_mode,
            extra_headers,
            organization,
            project,
            service_tier,
            deployment_name_map: BTreeMap::new(),
            transport,
        }
    }

    /// Resolve a caller-supplied model id to the Azure-deployment name the
    /// provider should send in the request body. Falls back to the input
    /// model id when no entry is present, preserving the historical
    /// "deployment id is the model id" behavior for users who never
    /// configured `[providers.azure_openai.deployment_name_map]`.
    ///
    /// The lookup is exact-match; Azure deployment ids are case-sensitive
    /// and the surrounding registry already canonicalizes model ids to
    /// their on-disk shape, so we do not lowercase here.
    pub(crate) fn resolve_deployment_name<'a>(&'a self, model: &'a str) -> &'a str {
        resolve_deployment_name(&self.deployment_name_map, model)
    }

    fn apply_openai_metadata_headers(
        &self,
        mut builder: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        if let Some(value) = self.organization.as_deref() {
            builder = builder.header("OpenAI-Organization", value);
        }
        if let Some(value) = self.project.as_deref() {
            builder = builder.header("OpenAI-Project", value);
        }
        builder
    }

    fn apply_service_tier(&self, body: &mut Value) {
        if let Some(service_tier) = self.service_tier.as_deref() {
            body["service_tier"] = json!(service_tier);
        }
    }

    pub(crate) fn request_body(request: &LlmRequest, provider_name: &str) -> Value {
        // Canonicalize cross-provider tool-call ids and synthesize
        // placeholders for orphan tool results BEFORE projecting to
        // the Responses-API `input` array. The Responses backend
        // matches `function_call_output.call_id` against the prior
        // `function_call.call_id` in the input slice; if the user
        // switched from Anthropic/Google/Bedrock mid-session those
        // ids carry the upstream's shape (`toolu_…`,
        // `google_call_…`, `tooluse_…`) and the pairing breaks even
        // though OpenAI itself accepts the id shape.
        let normalized_input = crate::normalize_tool_ids_for_replay(&request.input);
        // H-37 (Azure default): Azure's Responses API requires
        // `store: true` for the multi-turn `previous_response_id` flow
        // (the prior response must persist on the server). Codex's
        // client mirrors this with
        // `store: provider.is_azure_responses_endpoint()`
        // (`others/codex/codex-rs/core/src/client.rs:761`).
        // Honor the caller's explicit setting if they passed `true`;
        // otherwise default to `true` for Azure regardless of the
        // request-side `store` slot. Non-Azure providers keep the
        // caller's verbatim value.
        let effective_store = if provider_name == "azure_openai" {
            // Caller already opted in (or already opted out via the
            // explicit `store: true/false`). We can't distinguish a
            // caller-supplied `false` from the default `false` at this
            // layer without a schema add, so default to `true` for
            // Azure and document the override in the docstring above.
            true
        } else {
            request.store
        };
        let mut body = json!({
            "model": request.model,
            "input": openai_input(&normalized_input),
            "stream": true,
            "store": effective_store,
        });
        // M-02: only emit `instructions` when non-empty. An empty string
        // would shadow the stored conversation default on a
        // `previous_response_id` chain (codex mirrors this with
        // `#[serde(skip_serializing_if = "String::is_empty")]`).
        if !request.instructions.is_empty() {
            // OpenAI caches on a prefix hash and cannot be disabled
            // server-side, so a per-request nonce at the very front of the
            // (cacheable) `instructions` prefix forces a cold, full-priced
            // prefill every turn when caching is suppressed.
            if request.disable_prompt_cache {
                body["instructions"] = json!(format!(
                    "[cache-bust:{}]\n{}",
                    cache_bust_nonce(),
                    request.instructions
                ));
            } else {
                body["instructions"] = json!(request.instructions);
            }
        }
        if let Some(previous_response_id) = &request.previous_response_id {
            body["previous_response_id"] = json!(previous_response_id);
        }
        if request.disable_prompt_cache {
            // Unique key so cache affinity never routes this turn to a
            // warmed prefix; pairs with the instructions nonce above to
            // guarantee a cold prefill.
            body["prompt_cache_key"] = json!(format!("nocache-{}", cache_bust_nonce()));
        } else if let Some(key) = request.effective_cache_key() {
            // OpenAI's Responses API silently drops `prompt_cache_key`
            // values longer than 64 codepoints — the request succeeds
            // but the field is ignored server-side, so every turn pays
            // full uncached input cost while telemetry shows zero
            // cache hits. Clamp client-side. See [`clamp_prompt_cache_key`].
            body["prompt_cache_key"] = json!(clamp_prompt_cache_key(key));
        }
        if request.effective_cache_retention() == crate::CacheRetention::Long {
            body["prompt_cache_retention"] = json!("24h");
        }
        if let Some(max_output_tokens) = request.max_output_tokens {
            body["max_output_tokens"] = json!(max_output_tokens);
        }
        let mut text = serde_json::Map::new();
        if let Some(response_verbosity) = request.response_verbosity {
            text.insert(
                "verbosity".to_string(),
                json!(openai_text_verbosity(response_verbosity)),
            );
        }
        if let Some(schema) = request.output_schema.as_ref() {
            text.insert("format".to_string(), openai_text_format(schema));
        }
        if !text.is_empty() {
            body["text"] = Value::Object(text);
        }
        // The o-series and gpt-5.x models reason internally regardless of
        // whether the caller picks an effort level; the API just won't stream
        // a summary or expose `encrypted_content` for replay unless we ask
        // for it. Request both whenever the model registry says this model
        // supports reasoning, and only forward `effort` when the caller
        // explicitly set one (else the provider uses its own per-model
        // default).
        let reasoning_capable = crate::capabilities_for(provider_name, &request.model)
            .is_some_and(|caps| caps.reasoning_effort);
        if reasoning_capable || request.reasoning_effort.is_some() {
            let mut reasoning = serde_json::Map::new();
            reasoning.insert("summary".to_string(), json!("auto"));
            if let Some(effort) = request.reasoning_effort {
                reasoning.insert("effort".to_string(), json!(effort.as_str()));
            }
            body["reasoning"] = Value::Object(reasoning);
            if !request.store {
                body["include"] = json!(["reasoning.encrypted_content"]);
            }
        }
        if !request.tools.is_empty() {
            let mut tools = Vec::with_capacity(request.tools.len());
            for tool in request.tools.iter() {
                tools.push(json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                    "strict": tool.strict,
                }));
            }
            body["tools"] = Value::Array(tools);
        }
        // M-03: forward `tool_choice` unconditionally — Responses-state
        // continuations re-attach the prior turn's tools via
        // `previous_response_id`, so a caller saying
        // `tool_choice: "none"` on a follow-up turn needs the field even
        // when `request.tools` is empty. `None` omits the field and
        // falls back to the provider's `auto` default.
        if let Some(choice) = request.tool_choice.as_deref() {
            body["tool_choice"] = json!(choice);
        }
        if let Some(parallel) = request.parallel_tool_calls {
            // OpenAI's Responses API defaults to parallel tool calls; only
            // forward an explicit override when the caller opts out
            // (`Some(false)`) so the request body stays compact for the
            // common case.
            if !parallel {
                body["parallel_tool_calls"] = json!(false);
            }
        }
        body
    }

    /// Prompt-cache affinity headers attached to every Responses request
    /// that carries a cache key. OpenAI's load balancer uses these to
    /// route the session to the same backend that warmed the cached
    /// prefix; without them, repeat turns can land on a cold node and
    /// silently miss cache even when `prompt_cache_key` matches. The
    /// header values are `session_id` and `x-client-request-id`, both
    /// taken from the request's cache key.
    ///
    /// The header values carry up to 256 bytes of the cache key — the
    /// 64-codepoint limit is specific to the body field. Defensive
    /// 256-byte clamp (audit LOW): reqwest/hyper enforce an 8KB header
    /// line cap and adversarial inputs (multi-MB cache keys propagated
    /// from user-controlled session ids) would panic the request builder
    /// before the cap kicks in.
    pub(crate) fn affinity_headers(request: &LlmRequest) -> Vec<(&'static str, String)> {
        let Some(key) = request.effective_cache_spec().key else {
            return Vec::new();
        };
        let clamped = clamp_affinity_header_value(&key);
        vec![
            ("session_id", clamped.clone()),
            ("x-client-request-id", clamped),
        ]
    }
}

/// Process-unique nonce used to bust OpenAI's automatic prompt-prefix
/// cache when `disable_prompt_cache` is set. Combines a monotonic counter
/// (uniqueness within the process, even for same-nanosecond parallel
/// requests) with the wall-clock nanosecond (uniqueness across processes
/// in a parallel eval sweep) so no two requests ever share a cacheable
/// prefix.
fn cache_bust_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{n:x}")
}

/// Clamp a cache-key value to 256 bytes for use as an HTTP header.
/// Splits on a UTF-8 codepoint boundary so the result is still a valid
/// `String`. Values already at or under the cap pass through verbatim.
fn clamp_affinity_header_value(value: &str) -> String {
    const MAX_BYTES: usize = 256;
    if value.len() <= MAX_BYTES {
        return value.to_string();
    }
    let mut end = MAX_BYTES;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn openai_text_verbosity(verbosity: ResponseVerbosity) -> &'static str {
    match verbosity {
        ResponseVerbosity::Concise => "low",
        ResponseVerbosity::Normal => "medium",
        ResponseVerbosity::Verbose => "high",
    }
}

/// Translate a logical model id into an Azure-deployment id using
/// `map`. Unmapped ids fall through verbatim so callers without a
/// configured `deployment_name_map` keep the historical contract where
/// `[model].name` *is* the deployment id. Split out as a free function
/// so unit tests can exercise the substitution table without
/// constructing a real provider/client.
pub(crate) fn resolve_deployment_name<'a>(
    map: &'a BTreeMap<String, String>,
    model: &'a str,
) -> &'a str {
    map.get(model).map(String::as_str).unwrap_or(model)
}

fn openai_text_format(schema: &LlmOutputSchema) -> Value {
    json!({
        "type": "json_schema",
        "name": schema.name,
        "strict": schema.strict,
        "schema": schema.schema,
    })
}

/// Build the `/responses` URL for a single turn.
///
/// - Standard OpenAI / xAI / non-classic Azure: appends `/responses` to
///   the base URL (already trimmed of trailing `/`).
/// - Classic Azure (`/openai/deployments/{deployment}` URL shape, H-36):
///   the deployment is already in the path so `/responses` still
///   appends.
/// - `api_version`: when `Some`, attaches as a query parameter using
///   `&` if the base URL already carries a query string (AZ-M4) and
///   percent-encoding the value (AZ-M4 / M-56) so typos like
///   `"preview "` produce a well-formed URL instead of an invalid HTTP
///   request.
fn build_responses_url(base_url: &str, api_version: Option<&str>, _classic_azure: bool) -> String {
    // The classic-Azure flag is currently informational — both shapes
    // append `/responses`; the deployment-in-path lives in `base_url`
    // already. Held in the signature so a future per-shape divergence
    // (e.g. `/responses` vs `/responses?api-version=…` dating per
    // Azure quickstart) doesn't break the call-site contract.
    // If the base already carries a query string (e.g. an Azure base_url with
    // `?subscription-key=…`), `/responses` must be appended to the PATH, not
    // concatenated onto the query value — otherwise the path segment lands
    // inside the query and the request hits the wrong endpoint. Split any
    // existing query off, append the path segment, then re-attach the existing
    // query params plus `api-version`.
    let (path, existing_query) = match base_url.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (base_url, None),
    };
    let mut url = format!("{path}/responses");
    let mut query_parts: Vec<String> = Vec::new();
    if let Some(query) = existing_query.filter(|q| !q.is_empty()) {
        query_parts.push(query.to_string());
    }
    if let Some(api_version) = api_version {
        query_parts.push(format!(
            "api-version={}",
            percent_encode_query_component(api_version)
        ));
    }
    if !query_parts.is_empty() {
        url.push('?');
        url.push_str(&query_parts.join("&"));
    }
    url
}

/// Minimal RFC 3986 query-component percent-encoder for `api-version`.
/// Encodes everything outside the unreserved set
/// (`A-Z` / `a-z` / `0-9` / `-` `_` `.` `~`) so that valid Azure
/// `api-version` values (`preview`, `2024-10-21`) pass through verbatim
/// while user typos (trailing space, `?`, `&`, `#`, …) become
/// percent-escapes instead of breaking the URL structure.
fn percent_encode_query_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

fn azure_headers_provide_auth(headers: &BTreeMap<String, String>) -> bool {
    headers.keys().any(|key| {
        key.eq_ignore_ascii_case("authorization")
            || key.eq_ignore_ascii_case("api-key")
            || key.eq_ignore_ascii_case("apim-subscription-key")
            || key.eq_ignore_ascii_case("ocp-apim-subscription-key")
    })
}

impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
        if let Err(err) = request.ensure_vision_support(self.name) {
            return Box::pin(futures_util::stream::once(async move { Err(err) }));
        }
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let provider_name = self.name;
        let transport = self.transport;
        // H-36 + AZ-M4: build the `/responses` URL via a small helper so
        // (a) the classic `/openai/deployments/{deployment}` shape skips
        // the path rewrite, and (b) `?api-version=` composition stays
        // safe when the api_version string contains reserved chars or
        // the base_url already has a query string. Encoding via
        // `url::form_urlencoded::byte_serialize` follows RFC 3986
        // `query` rules.
        let url = build_responses_url(
            &self.base_url,
            self.api_version.as_deref(),
            self.is_classic_azure_deployment_url(),
        );
        let mut body = Self::request_body(&request, provider_name);
        // Azure resources expose deployments under user-chosen ids (e.g.
        // `my-deployment-gpt-4o`). The Responses route reads the body's
        // `model` field as the deployment selector, so callers who keep
        // logical OpenAI model ids in `[model]` must have them rewritten
        // here. The map is empty for the OpenAI / Codex / xAI constructors,
        // making this a no-op in those paths. Capability lookups above
        // intentionally ran against the *original* `request.model` so
        // the reasoning/effort decision still uses our static registry.
        //
        // H-36: the classic `/openai/deployments/{deployment}/responses`
        // URL shape carries the deployment in the path, so the body
        // model still matters for telemetry but does not select the
        // deployment. The body-side rewrite still runs (no harm) and
        // the URL-build above kept the user's `base_url` unmodified.
        let deployment = self.resolve_deployment_name(&request.model);
        if deployment != request.model.as_ref() {
            body["model"] = json!(deployment);
        }
        self.apply_service_tier(&mut body);
        let affinity_headers = Self::affinity_headers(&request);
        let auth_mode = self.auth_mode;
        let extra_headers = self.extra_headers.clone();
        let provider = self.clone();

        Box::pin(try_stream! {
            let response = send_with_auth_retry(
                &api_key,
                RetryPolicy::provider_requests(transport),
                &cancel,
                |key| {
                    let builder = client.post(&url);
                    let builder = match auth_mode {
                        OpenAiAuthMode::ApiKey => builder.header("api-key", key),
                        OpenAiAuthMode::Bearer => builder.bearer_auth(key),
                        OpenAiAuthMode::HeadersOnly => builder,
                    };
                    let builder = provider.apply_openai_metadata_headers(builder);
                    // Cache-affinity headers (only emitted when the
                    // request carries a cache key) keep multi-turn
                    // sessions pinned to the backend that warmed the
                    // cached prefix.
                    let builder = affinity_headers
                        .iter()
                        .fold(builder, |b, (name, value)| b.header(*name, value.as_str()));
                    let builder = extra_headers
                        .iter()
                        .fold(builder, |b, (name, value)| b.header(name, value));
                    builder.json(&body)
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

            yield LlmEvent::Started;

            let mut decoder = SseDecoder::default();
            let mut saw_completed = false;
            let mut reasoning_acc = ReasoningAccumulator::default();
            let mut server_model_echo = crate::ServerModelEcho::default();
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
                    SqueezyError::ProviderStream("OpenAI stream idle timeout".to_string())
                })?;
                let Some(chunk) = next else { break; };
                let chunk = chunk.map_err(|err| SqueezyError::ProviderStream(err.to_string()))?;
                for event in decoder.push(&chunk) {
                    let parsed = parse_openai_event(&event, &mut reasoning_acc);
                    // H-06: drain pre-yield (`ContextOverflow` etc.)
                    // *before* propagating either the parsed event or a
                    // terminal error so the agent's recovery layer sees
                    // the structured signal first.
                    for pre in reasoning_acc.drain_pre_yield() {
                        yield pre;
                    }
                    let parsed = parsed?;
                    if let Some(server) = reasoning_acc.take_server_model()
                        && let Some(echo) = server_model_echo.observe(&request.model, &server)
                    {
                        yield echo;
                    }
                    if let Some(llm_event) = parsed {
                        if matches!(llm_event, LlmEvent::Completed { .. }) {
                            saw_completed = true;
                        }
                        yield llm_event;
                    }
                }
            }

            for event in decoder.finish() {
                let parsed = parse_openai_event(&event, &mut reasoning_acc);
                for pre in reasoning_acc.drain_pre_yield() {
                    yield pre;
                }
                let parsed = parsed?;
                if let Some(server) = reasoning_acc.take_server_model()
                    && let Some(echo) = server_model_echo.observe(&request.model, &server)
                {
                    yield echo;
                }
                if let Some(llm_event) = parsed {
                    if matches!(llm_event, LlmEvent::Completed { .. }) {
                        saw_completed = true;
                    }
                    yield llm_event;
                }
            }

            if !saw_completed {
                Err(SqueezyError::ProviderStream(
                    "OpenAI stream ended without response.completed".to_string(),
                ))?;
            }
        })
    }
}

/// Buffers reasoning deltas across an OpenAI Responses stream so
/// `response.output_item.done` can backfill an empty `summary` from
/// the streamed text. The Responses API sometimes ships the item-close
/// event with `summary: []` even though `response.reasoning_summary_text.delta`
/// (or `response.reasoning_text.delta`) already streamed the real text;
/// without the buffer the persisted `ReasoningPayload::OpenAi.summary`
/// would be empty and the next turn's replay would drop the segment.
#[derive(Debug, Default)]
pub(crate) struct ReasoningAccumulator {
    summary: String,
    text: String,
    /// Stashes the server-echoed `response.model` field the first time
    /// any event carries it (typically `response.created`). The outer
    /// stream loop drains the value via [`Self::take_server_model`] and
    /// feeds it to [`crate::ServerModelEcho`] so [`LlmEvent::ServerModel`]
    /// lands additively right after [`LlmEvent::Started`]. Stays `None`
    /// for every event that doesn't include a `response.model` field —
    /// the loop only acts on the first observation per stream.
    server_model: Option<String>,
    /// Cumulative streamed text from `response.output_text.delta` events.
    /// Used by `response.output_text.done` (H-08) to reconcile against
    /// the authoritative final string and emit a corrective `TextDelta`
    /// for any suffix divergence (re-ordering or dropped delta during
    /// reconnect-without-skip).
    text_buffer: String,
    /// `true` once a `response.refusal.delta` or `response.refusal.done`
    /// has been observed in this stream. Drives the C-02 normalization:
    /// the terminal `response.completed` event normally lands with no
    /// `incomplete_details` (refusals ARE the completion, not an
    /// incomplete state), so we override `stop_reason` to
    /// `StopReason::Refusal` here. The TUI also sees per-delta refusal
    /// text via `LlmEvent::Refusal` while the refusal streams.
    refusal_latched: bool,
    /// Per-item-id (call_id) cache of the function name observed on the
    /// matching `response.output_item.added` event. Needed because
    /// `response.function_call_arguments.delta` (H-07) carries only the
    /// `item_id` + chunk text, not the function name — downstream
    /// consumers need the name to render a meaningful "calling tool X"
    /// hint. The terminal `response.output_item.done` event still
    /// produces the canonical `LlmEvent::ToolCall` with the fully
    /// assembled arguments, so consumers may also ignore the deltas.
    tool_call_names: std::collections::HashMap<String, String>,
    /// Events the parser wants to yield *before* the next return value
    /// fires (typically an [`LlmEvent::ContextOverflow`] preceding the
    /// `response.failed` terminal error — H-06). The outer stream loop
    /// drains this after every `parse_openai_event` call (including the
    /// error path) so the signal reaches consumers before the
    /// `SqueezyError::ProviderStream` terminal value lands.
    pre_yield: std::collections::VecDeque<LlmEvent>,
}

impl ReasoningAccumulator {
    fn take(&mut self) -> Vec<String> {
        let mut out = Vec::with_capacity(2);
        let summary = std::mem::take(&mut self.summary);
        if !summary.is_empty() {
            out.push(summary);
        }
        let text = std::mem::take(&mut self.text);
        if !text.is_empty() {
            out.push(text);
        }
        out
    }

    pub(crate) fn take_server_model(&mut self) -> Option<String> {
        self.server_model.take()
    }

    /// Drain queued events the parser wants to surface ahead of the
    /// next return value. Used by H-06's `response.failed` path to
    /// emit a [`LlmEvent::ContextOverflow`] before the terminal
    /// `SqueezyError::ProviderStream` value lands. The outer stream
    /// loop calls this after every `parse_openai_event` invocation —
    /// including the error path — so the pre-yield signal always
    /// reaches consumers.
    pub(crate) fn drain_pre_yield(&mut self) -> std::collections::vec_deque::IntoIter<LlmEvent> {
        std::mem::take(&mut self.pre_yield).into_iter()
    }
}

pub(crate) fn parse_openai_event(
    data: &str,
    reasoning_acc: &mut ReasoningAccumulator,
) -> Result<Option<LlmEvent>> {
    // Q11 removed the `[DONE]` sentinel — Responses API never emits it
    // (only Chat Completions does). If a malformed proxy ever injects
    // it the empty-data parse error path below surfaces it.

    let value: Value = serde_json::from_str(data)
        .map_err(|err| SqueezyError::ProviderStream(format!("invalid SSE JSON: {err}")))?;
    // LOW: validate `event.type` is actually a string. A malformed proxy
    // that ships `{"type": null, ...}` would otherwise silently land in
    // the `_ =>` "unhandled" arm. Track via a tracing warn so the
    // protocol violation is observable in a debug build.
    let event_type = match value.get("type") {
        Some(Value::String(s)) => s.as_str(),
        Some(other) => {
            tracing::warn!(
                target: "squeezy_llm::openai",
                ?other,
                "OpenAI SSE event carried a non-string `type` field",
            );
            ""
        }
        None => "",
    };
    // Q10: skip the trace line when the event has no type — useful for
    // hand-rolled debug fixtures but adds noise in production.
    if !event_type.is_empty() {
        tracing::trace!(target: "squeezy_llm::openai", event_type, "sse event");
    }

    // OpenAI Responses events embed the server-chosen model on the
    // `response` object that ships with `response.created`,
    // `response.in_progress`, `response.completed`, etc. Pluck it the
    // first time we see it so the outer stream loop can compare
    // against `request.model` and emit an additive `ServerModel`
    // event. Repeated observations on the same stream are coalesced
    // into a single drain in the caller via
    // [`crate::ServerModelEcho`].
    if reasoning_acc.server_model.is_none()
        && let Some(server_model) = value
            .get("response")
            .and_then(|response| response.get("model"))
            .and_then(Value::as_str)
        && !server_model.is_empty()
    {
        reasoning_acc.server_model = Some(server_model.to_string());
    }

    match event_type {
        "response.output_text.delta" => {
            let delta = value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            // Cumulative buffer feeds the H-08 `output_text.done`
            // reconcile path below.
            reasoning_acc.text_buffer.push_str(&delta);
            Ok(Some(LlmEvent::TextDelta(delta)))
        }
        "response.output_text.done" => {
            // H-08: the `.done` event carries the authoritative final
            // string for the completed text part. Reconcile against the
            // cumulative delta buffer — if a delta was dropped during a
            // mid-stream reconnect-without-skip or arrived out of order,
            // emit a corrective `TextDelta` for the missing suffix so
            // the persisted transcript matches what the model actually
            // produced. Common case: deltas matched, no event emitted.
            let authoritative = value.get("text").and_then(Value::as_str).unwrap_or("");
            let already = reasoning_acc.text_buffer.as_str();
            if let Some(suffix) = authoritative.strip_prefix(already)
                && !suffix.is_empty()
            {
                let suffix = suffix.to_string();
                reasoning_acc.text_buffer.push_str(&suffix);
                return Ok(Some(LlmEvent::TextDelta(suffix)));
            }
            Ok(None)
        }
        "response.refusal.delta" => {
            // C-02: safety-refusal text streams through `refusal.delta`
            // chunks ending with `refusal.done`. Surface each delta as a
            // typed `Refusal` event so the TUI shows the running refusal
            // text, and latch the flag so the terminal `response.completed`
            // (which arrives without `incomplete_details`) normalizes to
            // `StopReason::Refusal` instead of `EndTurn`.
            let delta = value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            reasoning_acc.refusal_latched = true;
            Ok(Some(LlmEvent::Refusal { content: delta }))
        }
        "response.refusal.done" => {
            // Latch even when no `delta` event preceded the done (defensive
            // — production streams always start with at least one delta).
            reasoning_acc.refusal_latched = true;
            Ok(None)
        }
        "response.output_item.added" => {
            // H-07: pre-register the call_id → name mapping the
            // subsequent `response.function_call_arguments.delta`
            // events need to surface a meaningful name. The terminal
            // `response.output_item.done` still emits the canonical
            // `ToolCall` with the assembled arguments so consumers
            // that only watch the final event keep working.
            if let Some(item) = value.get("item")
                && item.get("type").and_then(Value::as_str) == Some("function_call")
                && let (Some(item_id), Some(name)) = (
                    item.get("id").and_then(Value::as_str),
                    item.get("name").and_then(Value::as_str),
                )
            {
                reasoning_acc
                    .tool_call_names
                    .insert(item_id.to_string(), name.to_string());
            }
            Ok(None)
        }
        "response.function_call_arguments.delta" => {
            // H-07: incremental tool-arguments delta. OpenAI's Responses
            // streams `apply_patch` / multi-file diff arguments
            // chunk-by-chunk; surfacing them lets the UI show progress
            // before the full call materializes. The canonical
            // `LlmEvent::ToolCall` still lands at `output_item.done`.
            let item_id = value
                .get("item_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let chunk = value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = reasoning_acc
                .tool_call_names
                .get(&item_id)
                .cloned()
                .unwrap_or_default();
            Ok(Some(LlmEvent::ToolCallDelta {
                call_id: item_id,
                name,
                arguments_chunk: chunk,
            }))
        }
        "response.reasoning_summary_text.delta" => {
            let delta = value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            reasoning_acc.summary.push_str(&delta);
            Ok(Some(LlmEvent::ReasoningDelta {
                text: delta,
                kind: ReasoningKind::Summary,
            }))
        }
        "response.reasoning_text.delta" => {
            let delta = value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            reasoning_acc.text.push_str(&delta);
            Ok(Some(LlmEvent::ReasoningDelta {
                text: delta,
                kind: ReasoningKind::Text,
            }))
        }
        "response.output_item.done" => {
            let item = value.get("item");
            if let Some(payload) = parse_reasoning_item(item, reasoning_acc) {
                Ok(Some(LlmEvent::ReasoningDone(payload)))
            } else if let Some(tool_call) = parse_tool_call(item)? {
                Ok(Some(LlmEvent::ToolCall(tool_call)))
            } else {
                Ok(None)
            }
        }
        "response.completed" => {
            let response = value.get("response");
            let response_id = response
                .and_then(|response| response.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            // Successful completions don't carry `incomplete_details`;
            // treat their absence as `EndTurn` so the agent's turn loop
            // sees a normalized stop reason on every provider event.
            // C-02: when a `response.refusal.delta` latched earlier in
            // the same stream, override to `Refusal` — the terminal
            // event arrives without `incomplete_details` because the
            // refusal text IS the completion, not an incomplete state.
            let stop_reason = if reasoning_acc.refusal_latched {
                Some(crate::StopReason::Refusal)
            } else {
                response
                    .and_then(|response| response.get("incomplete_details"))
                    .and_then(|details| details.get("reason"))
                    .and_then(Value::as_str)
                    .map(crate::StopReason::from_openai_incomplete)
                    .or(Some(crate::StopReason::EndTurn))
            };
            Ok(Some(LlmEvent::Completed {
                response_id,
                cost: parse_cost(response),
                stop_reason,
                reasoning_only_stop: false,
            }))
        }
        "response.incomplete" => {
            // Surface incompletion to the agent instead of erroring here
            // so max-output-tokens / content-filter cases reach the same
            // recovery path as the other providers.
            let response = value.get("response");
            let response_id = response
                .and_then(|response| response.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let incomplete_details =
                response.and_then(|response| response.get("incomplete_details"));
            let reason = incomplete_details
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str);
            // C-14 (Azure mid-stream): when the output filter blocks a
            // response after streaming starts, Azure surfaces
            // `incomplete_details.content_filter_result` with per-category
            // severity. Queue a `LlmEvent::Refusal` ahead of the
            // `Completed` event so the TUI shows the filter category
            // alongside the canonical `StopReason::Refusal` signal.
            if reason == Some("content_filter") {
                let filter_result =
                    incomplete_details.and_then(|details| details.get("content_filter_result"));
                if let Some(result) = filter_result {
                    let summary = azure_content_filter_categories(result);
                    reasoning_acc
                        .pre_yield
                        .push_back(LlmEvent::Refusal { content: summary });
                }
                reasoning_acc.refusal_latched = true;
            }
            let stop_reason = reason
                .map(crate::StopReason::from_openai_incomplete)
                .or(Some(crate::StopReason::Other("incomplete".to_string())));
            Ok(Some(LlmEvent::Completed {
                response_id,
                cost: parse_cost(response),
                stop_reason,
                reasoning_only_stop: false,
            }))
        }
        "error" | "response.failed" => {
            // H-06: parse the `error.{code,message,param}` envelope and
            // branch on `code` so the agent's recovery layer sees a
            // structured signal instead of a flat stringified error.
            // `response.failed.response.error` is the canonical envelope
            // (codex models the same shape in `codex-rs/codex-api/src/sse/responses.rs`).
            // Stale-`previous_response_id` (M-05) is handled here too —
            // surfaced with a `previous_response_not_found:` marker prefix
            // so the agent layer can detect it without a SqueezyError
            // schema add (squeezy-core scope, Phase 4I).
            let response_error = value
                .get("response")
                .and_then(|response| response.get("error"));
            let error_obj = response_error
                .or_else(|| value.get("error"))
                .cloned()
                .unwrap_or(Value::Null);
            let code = error_obj
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let message = error_obj
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| value.get("message").and_then(Value::as_str))
                .unwrap_or("OpenAI stream error");
            let param = error_obj.get("param").and_then(Value::as_str);

            // Stitch a descriptive prefix that downstream marker-prefix
            // detectors (M-05 stale `previous_response_id`, agent's
            // overflow recovery) can match without a SqueezyError schema
            // add. Falls back to the bare provider message when no
            // recognised code lands.
            let prefixed = match code {
                "context_length_exceeded" => {
                    // Push the canonical `ContextOverflow` event ahead of
                    // the terminal error so the agent's compact-and-
                    // retry recovery fires before the bare provider
                    // error reaches the turn loop.
                    reasoning_acc
                        .pre_yield
                        .push_back(LlmEvent::ContextOverflow {
                            provider: "openai".to_string(),
                            signal: crate::OverflowSignal::ErrorPattern(message.to_string()),
                        });
                    format!("context_length_exceeded: {message}")
                }
                "content_filter" => {
                    // C-14 (Azure): the content-filter envelope ships
                    // per-category severity inside
                    // `error.innererror.content_filter_result.{hate,sexual,
                    // violence,self_harm,jailbreak,protected_material_*}`.
                    // Surface a `LlmEvent::Refusal` ahead of the terminal
                    // error so the TUI shows the filter category instead
                    // of a bare 400. The summary string concatenates the
                    // filtered categories so the agent's transcript
                    // records *why* the refusal happened.
                    let summary = azure_content_filter_summary(&error_obj);
                    reasoning_acc.pre_yield.push_back(LlmEvent::Refusal {
                        content: summary.clone(),
                    });
                    reasoning_acc.refusal_latched = true;
                    format!("content_filter: {summary} ({message})")
                }
                "rate_limit_exceeded" => {
                    // Embed any "try again in 3s" hint the upstream
                    // ships in the message so the agent's retry layer
                    // can honor the Retry-After-style delay.
                    format!("rate_limit_exceeded: {message}")
                }
                "insufficient_quota" => format!("insufficient_quota: {message}"),
                "cyber_policy" => format!("cyber_policy: {message}"),
                "previous_response_not_found" => {
                    // M-05: mark stale `previous_response_id` 404s so
                    // the agent layer can drop the stored id and resend
                    // the materialized input. Squeezy-core's
                    // `SqueezyError` schema add lives in Phase 4I; the
                    // marker prefix keeps detection working today.
                    format!("previous_response_not_found: {message}")
                }
                "" => message.to_string(),
                other => format!("{other}: {message}"),
            };
            // Stash `param` in the message tail when present so debug
            // dashboards keep the field that points at the offending
            // request slot.
            let final_message = match param {
                Some(p) if !p.is_empty() => format!("{prefixed} (param: {p})"),
                _ => prefixed,
            };
            Err(SqueezyError::ProviderStream(final_message))
        }
        _ => {
            tracing::debug!(
                target: "squeezy_llm::openai",
                event_type,
                "unhandled OpenAI SSE event"
            );
            Ok(None)
        }
    }
}

/// Pretty-print the Azure content-filter envelope into a human-readable
/// string for the `LlmEvent::Refusal { content }` payload. Searches the
/// canonical paths Azure uses (per-prompt and per-response variants).
fn azure_content_filter_summary(error_obj: &Value) -> String {
    // Try the most common Azure shapes in order:
    //   error.innererror.content_filter_result.{category}
    //   error.content_filter_result.{category}
    let candidates = [
        error_obj
            .get("innererror")
            .and_then(|v| v.get("content_filter_result")),
        error_obj.get("content_filter_result"),
    ];
    for candidate in candidates.into_iter().flatten() {
        let summary = azure_content_filter_categories(candidate);
        if !summary.is_empty() {
            return summary;
        }
    }
    "content_filter".to_string()
}

/// Extract a category-level summary from an Azure `content_filter_result`
/// JSON object. Returns a comma-separated list of `category[:severity]`
/// pairs for every category whose `filtered: true` flag is set.
fn azure_content_filter_categories(result: &Value) -> String {
    let Some(obj) = result.as_object() else {
        return String::new();
    };
    let mut out: Vec<String> = Vec::new();
    for (category, value) in obj {
        let filtered = value
            .get("filtered")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let severity = value.get("severity").and_then(Value::as_str);
        let detected = value
            .get("detected")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if filtered {
            match severity {
                Some(sev) if !sev.is_empty() => out.push(format!("{category}:{sev}")),
                _ => out.push(category.clone()),
            }
        } else if detected {
            out.push(format!("{category}:detected"));
        }
    }
    if out.is_empty() {
        // Some envelopes only carry `severity` without `filtered`; fall
        // back to listing categories whose severity is set above safe.
        for (category, value) in obj {
            if let Some(sev) = value.get("severity").and_then(Value::as_str)
                && !sev.is_empty()
                && sev != "safe"
            {
                out.push(format!("{category}:{sev}"));
            }
        }
    }
    out.join(", ")
}

fn openai_input(input: &[LlmInputItem]) -> Value {
    // UserText array shape (audit MEDIUM finding): always emit the
    // typed array form so multi-item turns mixing `UserText` and
    // `Image` produce a uniform shape. The string-form fast-path is
    // removed because (a) OpenAI is phasing it out for Responses, and
    // (b) it produced a different prompt-cache prefix than the array
    // form for otherwise-identical bodies.
    let mut items = Vec::with_capacity(input.len());
    for item in input {
        if let Some(value) = openai_input_item(item) {
            items.push(value);
        }
    }
    Value::Array(items)
}

fn openai_input_item(item: &LlmInputItem) -> Option<Value> {
    Some(match item {
        LlmInputItem::UserText(text) => json!({
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": text,
            }],
        }),
        LlmInputItem::AssistantText(text) => json!({
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": text,
            }],
        }),
        LlmInputItem::FunctionCall {
            call_id,
            name,
            arguments,
        } => json!({
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string()),
        }),
        LlmInputItem::FunctionCallOutput {
            call_id,
            output,
            content_parts,
            ..
        } => {
            // M-06: when the caller attached structured tool-result
            // parts (image returns from browser tools, multi-block
            // outputs), emit the Responses-API array form so the model
            // receives the images directly instead of through a
            // base64-stringified blob inside `output`. Falls back to
            // the plain string form when `content_parts` is `None`.
            if let Some(parts) = content_parts
                && !parts.is_empty()
            {
                let serialized: Vec<Value> = parts
                    .iter()
                    .map(|part| match part {
                        crate::ToolResultPart::Text { text } => json!({
                            "type": "input_text",
                            "text": text,
                        }),
                        crate::ToolResultPart::Image { media_type, bytes } => json!({
                            "type": "input_image",
                            "detail": "auto",
                            "image_url": format!(
                                "data:{media_type};base64,{}",
                                BASE64_STANDARD.encode(bytes.as_ref())
                            ),
                        }),
                    })
                    .collect();
                json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": Value::Array(serialized),
                })
            } else {
                json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                })
            }
        }
        LlmInputItem::Image { media_type, bytes } => json!({
            "role": "user",
            "content": [{
                "type": "input_image",
                "detail": "auto",
                "image_url": format!(
                    "data:{media_type};base64,{}",
                    BASE64_STANDARD.encode(bytes.as_ref())
                ),
            }],
        }),
        LlmInputItem::Reasoning(ReasoningPayload::OpenAi {
            item_id,
            summary,
            encrypted_content,
        }) => {
            let summary_value = Value::Array(
                summary
                    .iter()
                    .map(|text| json!({ "type": "summary_text", "text": text }))
                    .collect(),
            );
            let mut obj = json!({
                "type": "reasoning",
                "id": item_id,
                "summary": summary_value,
            });
            if let Some(encrypted) = encrypted_content {
                obj["encrypted_content"] = json!(encrypted);
            }
            obj
        }
        // Reasoning items from other providers are dropped when replaying to OpenAI.
        LlmInputItem::Reasoning(_) => return None,
        // OpenAI Responses accepts document inputs via the `input_file`
        // content block. Per-provider lowering lands in Phase 4; for
        // now we skip with a debug log so the request still ships.
        LlmInputItem::Document { name, .. } => {
            tracing::debug!(
                target: "squeezy_llm::openai",
                name = name.as_str(),
                "openai document content block not yet implemented; skipping",
            );
            return None;
        }
    })
}

fn parse_reasoning_item(
    item: Option<&Value>,
    reasoning_acc: &mut ReasoningAccumulator,
) -> Option<ReasoningPayload> {
    let item = item?;
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return None;
    }
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut summary: Vec<String> = item
        .get("summary")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str).map(str::to_string))
                .filter(|text| !text.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // OpenAI Responses sometimes closes the reasoning item with `summary: []`
    // even when `response.reasoning_summary_text.delta` already streamed the
    // real text; without backfilling from the streamed deltas the persisted
    // payload would be empty and the next turn's replay would lose the
    // segment. Drain the accumulator on every `output_item.done` so a
    // subsequent reasoning item starts clean.
    let buffered = reasoning_acc.take();
    if summary.is_empty() {
        summary = buffered;
    }
    let encrypted_content = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(ReasoningPayload::OpenAi {
        item_id,
        summary,
        encrypted_content,
    })
}

fn parse_tool_call(item: Option<&Value>) -> Result<Option<LlmToolCall>> {
    let Some(item) = item else {
        return Ok(None);
    };
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return Ok(None);
    }

    let call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .or_else(|| item.get("id").and_then(Value::as_str))
        .ok_or_else(|| SqueezyError::ProviderStream("function call missing call_id".to_string()))?
        .to_string();
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| SqueezyError::ProviderStream("function call missing name".to_string()))?
        .to_string();
    let arguments = match item.get("arguments") {
        Some(Value::String(arguments)) => serde_json::from_str(arguments).unwrap_or_else(|err| {
            json!({
                INVALID_TOOL_ARGUMENTS_KEY: true,
                INVALID_TOOL_ARGUMENTS_ERROR_KEY: err.to_string(),
                INVALID_TOOL_ARGUMENTS_RAW_KEY: arguments,
            })
        }),
        Some(arguments @ Value::Object(_)) => arguments.clone(),
        None => Value::Object(Default::default()),
        Some(_) => {
            return Err(SqueezyError::ProviderStream(
                "function call arguments must be a JSON object or encoded JSON string".to_string(),
            ));
        }
    };

    Ok(Some(LlmToolCall {
        call_id,
        name,
        arguments,
    }))
}

fn parse_cost(response: Option<&Value>) -> CostSnapshot {
    let Some(usage) = response.and_then(|response| response.get("usage")) else {
        return CostSnapshot::default();
    };

    CostSnapshot {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        reasoning_output_tokens: usage
            .get("output_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64),
        cached_input_tokens: usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64),
        cache_write_input_tokens: None,
        estimated_usd_micros: None,
    }
}

#[cfg(test)]
#[path = "openai_tests.rs"]
mod tests;
