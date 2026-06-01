//! Provider-agnostic LLM request/event types and stream abstractions.
//!
//! # Prompt-cache hint deprecation
//!
//! [`LlmRequest::cache_key`] is retained for one release while callers
//! migrate to the universal [`LlmRequest::cache`] field. The legacy field
//! lifts into a [`CacheSpec`] via `From<Option<String>>` at the provider
//! boundary (see [`LlmRequest::effective_cache_spec`]): a `Some(key)` value
//! produces `{ key: Some(key), retention: Short }`, preserving the historical
//! 5m / in-memory provider-default behavior. New callers should set
//! `request.cache = CacheSpec { key, retention }` directly — in particular
//! [`CacheRetention::Long`] is the only path to Anthropic's 1h `ttl` and
//! OpenAI's `prompt_cache_retention: "24h"`. `cache_key` will be removed in
//! a subsequent release once all in-tree callers migrate.

use std::{pin::Pin, sync::Arc};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use futures_core::Stream;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
pub use squeezy_core::{
    AnthropicThinkingBlock, AnthropicThinkingKind, ReasoningKind, ReasoningPayload,
    ReasoningSnapshot, resolve_model_alias,
};
use squeezy_core::{CostSnapshot, ReasoningEffort, ResponseVerbosity, Result, SqueezyError};
use tokio_util::sync::CancellationToken;

pub const INVALID_TOOL_ARGUMENTS_KEY: &str = "__squeezy_invalid_tool_arguments";
pub const INVALID_TOOL_ARGUMENTS_ERROR_KEY: &str = "__squeezy_parse_error";
pub const INVALID_TOOL_ARGUMENTS_RAW_KEY: &str = "__squeezy_raw_arguments";

/// Tool name attached to a synthesized `FunctionCall` placeholder
/// that stands in for a missing tool call discovered during
/// cross-model replay (an orphan `FunctionCallOutput` whose `call_id`
/// has no matching `FunctionCall` in the prior history). Picked so
/// review tooling and exported transcripts can grep for the marker
/// rather than the gap appearing as a legitimate model invocation.
/// See [`normalize_tool_ids_for_replay`].
pub const MODEL_SWITCHED_PLACEHOLDER_NAME: &str = "model_switched";

mod anthropic;
mod anthropic_betas;
pub mod anthropic_error;
mod bedrock;
mod cache_policy;
mod compatible;
mod contribution;
mod credentials;
mod faux;
mod google;
mod lmstudio;
pub mod model_discovery;
pub mod models_dev;
pub mod oauth;
mod ollama;
mod openai;
mod openai_prompt_cache;
pub mod overflow;
mod registry;
mod retry;
mod sse;
pub mod tokens;
mod transport;
mod xai;
pub use tokens::{
    DEFAULT_BYTES_PER_TOKEN, DEFAULT_EMA_ALPHA, ProviderCalibration, TokenCalibration,
    default_bytes_per_token, estimate_tokens,
};

pub use anthropic::AnthropicProvider;
pub use bedrock::BedrockProvider;
pub use cache_policy::{CacheRetention, CacheSpec};
pub use compatible::OpenAiCompatibleProvider;
pub use contribution::{
    AnthropicContribution, AnthropicContributionConfig, GoogleContribution,
    GoogleContributionConfig, LoadedContributions, OllamaContribution, OllamaContributionConfig,
    OpenAiContribution, OpenAiContributionConfig, ProviderContribution, ProviderContributions,
};
pub use credentials::{
    ApiKeyFuture, ApiKeySource, KeySource, RefreshableToken, ResolvedKey, StaticApiKey, TokenState,
    delete_api_key, resolve_api_key, resolve_api_key_with_inline, static_api_key_source,
};
pub use faux::{DEFAULT_FAUX_NAME, FauxProvider, FauxScript, FauxStep, FauxToolCall, FauxTurn};
pub use google::GoogleProvider;
pub use lmstudio::{
    DEFAULT_LMSTUDIO_BASE_URL, LMStudioConfig, LMStudioProvider, fetch_lmstudio_model_names,
};
pub use model_discovery::{
    CONSERVATIVE_FALLBACK_CAPABILITIES, CapabilitySource, ResolvedCapabilities,
    resolve_capabilities, resolve_capabilities_with,
};
pub use oauth::{
    ANTHROPIC_OAUTH_TOKEN_PREFIX, AnthropicLoginConfig, AnthropicOAuthSource,
    DEFAULT_POLICY_MODELS, DevicePollOutcome, GITHUB_COPILOT_AUTH_FILE_NAME,
    GitHubCopilotDeviceCodeResponse, GitHubCopilotLoginHooks, GitHubCopilotLoginOutcome,
    GitHubCopilotOAuthSource, GitHubCopilotProvider, GitHubCopilotUrls,
    OPENAI_CODEX_AUTH_FILE_NAME, OpenAiCodexLoginOutcome, OpenAiCodexOAuthSource,
    OpenAiCodexProvider, PersistedGitHubCopilotTokens, PersistedTokens, PkceCodes,
    PolicyEnablementOutcome, TokenResponse,
    anthropic_default_storage_path as oauth_anthropic_default_storage_path,
    anthropic_oauth_beta_header, anthropic_read_tokens as oauth_anthropic_read_tokens,
    anthropic_write_tokens as oauth_anthropic_write_tokens, codex_auth_file_path,
    default_codex_auth_path, default_github_copilot_auth_path, enable_copilot_models,
    exchange_authorization_code, generate_pkce, github_copilot_auth_file_path,
    github_copilot_base_url_from_token, github_copilot_read_tokens, github_copilot_write_tokens,
    is_anthropic_oauth_token, load_codex_token, login_github_copilot_interactive,
    login_openai_codex_interactive, normalize_github_domain, parse_authorization_input,
    poll_for_github_token, refresh_anthropic_token, refresh_copilot_token,
    resolve_github_copilot_base_url, save_codex_token, start_github_copilot_device_flow,
};
pub use ollama::{
    OllamaProvider, PullEvent, PullStream, fetch_ollama_context_window, fetch_ollama_model_names,
    pull_model,
};
pub use openai::OpenAiProvider;
pub use overflow::{OverflowSignal, Usage as OverflowUsage, classify_terminal};
pub use registry::{
    MODEL_REGISTRY, ModelCapabilities, ModelInfo, ModelLifecycle, ModelLimits, PROVIDERS,
    RequestTokenEstimate, TokenPricing, TokenizerKind, capabilities_for, estimate_cost,
    estimate_request_context, estimate_request_context_calibrated, model_info_for,
    models_for_provider, provider_from_config, provider_name,
};
pub use xai::XaiProvider;

pub type LlmStream = Pin<Box<dyn Stream<Item = Result<LlmEvent>> + Send>>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmRequest {
    pub model: Arc<str>,
    pub instructions: Arc<str>,
    pub input: Arc<[LlmInputItem]>,
    pub max_output_tokens: Option<u32>,
    pub response_verbosity: Option<ResponseVerbosity>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub previous_response_id: Option<String>,
    /// **Deprecated.** Use [`LlmRequest::cache`] instead. Retained for one
    /// release as a backwards-compatibility shim: providers lift a
    /// `Some(key)` value into `CacheSpec { key: Some(key), retention: Short }`
    /// via [`LlmRequest::effective_cache_spec`], preserving the historical
    /// 5m / in-memory provider-default behavior.
    pub cache_key: Option<String>,
    /// Universal cache hint — affinity key plus retention window. Replaces
    /// the legacy [`LlmRequest::cache_key`] field. Set
    /// `retention: CacheRetention::Long` to opt into Anthropic's 1h `ttl`
    /// and OpenAI's `prompt_cache_retention: "24h"`. `Short` keeps the
    /// provider defaults; `None` disables caching entirely.
    #[serde(default)]
    pub cache: CacheSpec,
    pub tools: Arc<[Arc<LlmToolSpec>]>,
    pub store: bool,
    /// Optional `tool_choice` hint to forward to the provider when tools are
    /// advertised. `None` omits the field entirely — matches squeezy's
    /// historical behavior and lets the provider apply its default
    /// (typically `auto`). Set to `"required"` for tool-shy models like
    /// Qwen via OpenRouter that otherwise emit a chatty preamble and
    /// finish with `stop` without calling any tool.
    pub tool_choice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<LlmOutputSchema>,
    /// When `Some(false)`, force the OpenAI Responses API to issue tool
    /// calls serially. `None` leaves the OpenAI default (parallel) in
    /// place. Only the OpenAI provider currently reads this; other
    /// providers ignore it.
    pub parallel_tool_calls: Option<bool>,
    /// Anthropic beta opt-ins (e.g. `context-1m-2025-08-07`,
    /// `interleaved-thinking-2025-05-14`). Empty by default. The
    /// Anthropic provider joins these into an `anthropic-beta` HTTP
    /// header; the Bedrock provider partitions them and forwards only
    /// the body-param-eligible subset via
    /// `additional_model_request_fields.anthropic_beta`. Other
    /// providers ignore the field.
    #[serde(default = "empty_beta_headers")]
    pub beta_headers: Arc<[Arc<str>]>,
    /// Sampling temperature. `None` leaves the provider's default in
    /// place (matches squeezy's historical behavior). Per-provider
    /// lowering to the wire body lands in Phase 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Nucleus-sampling cutoff. `None` leaves the provider's default
    /// in place. Per-provider lowering lands in Phase 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Deterministic seed forwarded where the provider supports it
    /// (OpenAI Chat-Completions / Responses, Bedrock Converse `seed`,
    /// Ollama `options.seed`). `None` keeps generation
    /// non-deterministic. Per-provider lowering lands in Phase 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// Stop-sequence list (Anthropic `stop_sequences`, OpenAI `stop`,
    /// Google `stopSequences`, Bedrock `stopSequences`). Empty leaves
    /// the provider default. Per-provider lowering lands in Phase 4.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// OpenAI-style `frequency_penalty`. `None` keeps the default.
    /// Per-provider lowering lands in Phase 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    /// OpenAI-style `presence_penalty`. `None` keeps the default.
    /// Per-provider lowering lands in Phase 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    /// Provider-hosted tool registry. xAI Live Search (H-23),
    /// OpenAI's hosted web/file search, and Anthropic's computer-use
    /// tool live here so we can advertise them alongside the
    /// agent-defined [`LlmToolSpec`] list. Per-provider lowering
    /// lands in Phase 4.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosted_tools: Vec<Arc<LlmHostedTool>>,
}

/// Provider-hosted tool specification. Unlike [`LlmToolSpec`] (a
/// caller-defined function the agent will execute locally), these
/// describe tools the model invokes server-side: xAI Live Search,
/// OpenAI hosted web/file search, Anthropic computer-use, etc.
///
/// `#[non_exhaustive]` so future hosted tools (image-gen, code
/// interpreter, …) can land without breaking downstream `match`
/// statements.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LlmHostedTool {
    /// xAI Live Search / OpenAI `web_search_preview` style hosted
    /// search. `filters` carries provider-specific tuning
    /// (`allowed_domains`, `recency`, etc.) as opaque JSON.
    WebSearch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filters: Option<Value>,
    },
    /// OpenAI Responses `file_search` over a managed vector store.
    /// `vector_store_ids` lists the stores the model may search.
    FileSearch {
        #[serde(default)]
        vector_store_ids: Vec<String>,
    },
    /// Anthropic computer-use / OpenAI `computer_use_preview`. No
    /// parameters; presence advertises the capability.
    ComputerUse,
}

impl Default for LlmRequest {
    fn default() -> Self {
        Self {
            model: Arc::from(""),
            instructions: Arc::from(""),
            input: Arc::from(Vec::new()),
            max_output_tokens: None,
            response_verbosity: None,
            reasoning_effort: None,
            previous_response_id: None,
            cache_key: None,
            cache: CacheSpec::default(),
            tools: Arc::from(Vec::new()),
            store: false,
            tool_choice: None,
            output_schema: None,
            parallel_tool_calls: None,
            beta_headers: empty_beta_headers(),
            temperature: None,
            top_p: None,
            seed: None,
            stop: Vec::new(),
            frequency_penalty: None,
            presence_penalty: None,
            hosted_tools: Vec::new(),
        }
    }
}

fn empty_beta_headers() -> Arc<[Arc<str>]> {
    Arc::from(Vec::new())
}

impl LlmRequest {
    pub fn user_text(
        model: String,
        instructions: String,
        input: String,
        max_output_tokens: Option<u32>,
    ) -> Self {
        Self {
            model: Arc::from(model),
            instructions: Arc::from(instructions),
            input: Arc::from(vec![LlmInputItem::UserText(input)]),
            max_output_tokens,
            ..Self::default()
        }
    }

    /// Effective cache hint after lifting the deprecated
    /// [`LlmRequest::cache_key`] field into the new [`CacheSpec`] shape.
    ///
    /// The merge rule preserves backwards compatibility while letting new
    /// callers exclusively populate `cache`:
    /// - The explicit `cache.retention` always wins. Callers asking for
    ///   `CacheRetention::Long` get extended retention regardless of
    ///   whether they used the legacy `cache_key` slot for the key.
    /// - When `cache.key` is `None`, it inherits from `cache_key` so
    ///   legacy callers still get a `prompt_cache_key` on OpenAI routes.
    /// - When `cache.retention` is `None` *and* the legacy `cache_key` is
    ///   set, retention is lifted to `Short` so old callers preserve their
    ///   pre-retention-enum 5m / in-memory behavior.
    pub fn effective_cache_spec(&self) -> CacheSpec {
        let mut spec = self.cache.clone();
        if spec.key.is_none() {
            spec.key = self.cache_key.clone();
        }
        if spec.retention == CacheRetention::None && self.cache_key.is_some() {
            spec.retention = CacheRetention::Short;
        }
        spec
    }
}

/// Strict JSON Schema response contract carried on `LlmRequest::output_schema`.
///
/// Providers that support structured outputs (OpenAI Responses
/// `text.format = { type: "json_schema", ... }`) attach this to the request
/// body; others ignore it. `strict` mirrors OpenAI's "the model MUST emit
/// JSON that validates" flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmOutputSchema {
    pub name: String,
    pub schema: Value,
    pub strict: bool,
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum LlmInputItem {
    UserText(String),
    AssistantText(String),
    FunctionCall {
        call_id: String,
        name: String,
        arguments: Value,
    },
    /// Tool result returned to the model after a `FunctionCall`.
    ///
    /// `output` carries the canonical string form callers have always
    /// used (JSON-stringified output, terminal text, etc.) so existing
    /// providers and persistence stay byte-compatible. Two optional
    /// extensions land structured tool results without breaking the
    /// string contract:
    ///
    /// - `content_parts`: when `Some`, providers that support
    ///   structured tool results (Anthropic `tool_result.content`
    ///   arrays, Bedrock Converse `toolResult.content`, OpenAI
    ///   Responses image blocks) ship the part list directly instead
    ///   of round-tripping a base64-stringified image through
    ///   `output`. Each `ToolResultPart::Image` carries the raw bytes
    ///   as an `Arc<[u8]>` so a 110k-token base64 PNG never lands in
    ///   the wire body. Providers that don't support arrays fall back
    ///   to `output`.
    /// - `is_error`: Gemini's `functionResponse.response` switches its
    ///   shape between `{output}` and `{error}` based on whether the
    ///   tool failed; the flag carries that signal without forcing the
    ///   agent to embed a sentinel in `output`.
    FunctionCallOutput {
        call_id: String,
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_parts: Option<Vec<ToolResultPart>>,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
    Reasoning(ReasoningPayload),
    /// Inline image attached to a user turn. `media_type` is an
    /// `image/{png,jpeg,gif,webp}` MIME string; `bytes` carries the raw
    /// image payload (each provider's `request_body` re-encodes as
    /// needed — base64 data URL, `inlineData`, Bedrock `Blob`, etc.).
    /// Stored serialized as a base64 string so checkpoints stay JSON-
    /// safe without bloating to a byte array.
    Image {
        media_type: String,
        #[serde(serialize_with = "serialize_image_bytes_b64")]
        #[serde(deserialize_with = "deserialize_image_bytes_b64")]
        bytes: Arc<[u8]>,
    },
    /// Document attachment — PDF, DOCX, CSV, XLSX, etc. Bedrock
    /// Converse accepts document content blocks up to ~4.5 MB with a
    /// caller-supplied `name` and a MIME-typed payload. Anthropic
    /// Claude PDFs ride a similar `document` content block. Other
    /// providers route this item to a debug log + skip until a Phase 4
    /// lowering lands. `media_type` follows the same MIME convention
    /// as `Image`; `name` is the human-facing filename (Bedrock
    /// requires it, other providers persist it in transcript metadata).
    /// `bytes` round-trips as base64 like `Image::bytes`.
    Document {
        media_type: String,
        name: String,
        #[serde(serialize_with = "serialize_image_bytes_b64")]
        #[serde(deserialize_with = "deserialize_image_bytes_b64")]
        bytes: Arc<[u8]>,
    },
}

/// Structured content block carried inside
/// [`LlmInputItem::FunctionCallOutput::content_parts`].
///
/// Providers that accept array-shaped tool results (Anthropic, Bedrock
/// Converse, OpenAI Responses for image returns) lower this list
/// directly; providers that only accept a string `output` skip the
/// array and fall back to the `output` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultPart {
    Text {
        text: String,
    },
    Image {
        media_type: String,
        #[serde(serialize_with = "serialize_image_bytes_b64")]
        #[serde(deserialize_with = "deserialize_image_bytes_b64")]
        bytes: Arc<[u8]>,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl LlmInputItem {
    /// Construct an `Image` item from a media-type string and raw bytes.
    /// Convenience to keep call sites short.
    pub fn image(media_type: impl Into<String>, bytes: impl Into<Arc<[u8]>>) -> Self {
        Self::Image {
            media_type: media_type.into(),
            bytes: bytes.into(),
        }
    }

    /// Construct a `FunctionCallOutput` from a call id and string
    /// output, leaving `content_parts` empty and `is_error` false.
    /// Most callers want this shape — agents that stringify tool
    /// output into the legacy `output` slot, replay paths, and tests
    /// that only care about the call-id/output pair.
    pub fn function_output(call_id: impl Into<String>, output: impl Into<String>) -> Self {
        Self::FunctionCallOutput {
            call_id: call_id.into(),
            output: output.into(),
            content_parts: None,
            is_error: false,
        }
    }

    /// Construct a `Document` item from a MIME-typed payload and a
    /// human-facing filename. Mirrors [`Self::image`]'s shape;
    /// Bedrock's Converse API uses `name` directly while other
    /// providers stash it in transcript metadata.
    pub fn document(
        media_type: impl Into<String>,
        name: impl Into<String>,
        bytes: impl Into<Arc<[u8]>>,
    ) -> Self {
        Self::Document {
            media_type: media_type.into(),
            name: name.into(),
            bytes: bytes.into(),
        }
    }

    /// `true` for the `Image` variant. Used by the per-provider request
    /// builders and the vision-capability check below.
    pub fn is_image(&self) -> bool {
        matches!(self, Self::Image { .. })
    }

    /// `true` for the `Document` variant. Used by per-provider request
    /// builders to gate Bedrock's document content block path.
    pub fn is_document(&self) -> bool {
        matches!(self, Self::Document { .. })
    }
}

fn serialize_image_bytes_b64<S: Serializer>(
    bytes: &Arc<[u8]>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&BASE64_STANDARD.encode(bytes.as_ref()))
}

fn deserialize_image_bytes_b64<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Arc<[u8]>, D::Error> {
    use serde::de::Error;
    let encoded: String = String::deserialize(deserializer)?;
    let bytes = BASE64_STANDARD
        .decode(encoded.as_bytes())
        .map_err(|err| Error::custom(format!("invalid base64 image payload: {err}")))?;
    Ok(Arc::from(bytes.into_boxed_slice()))
}

/// Detect the canonical image MIME type from a byte prefix using magic
/// numbers. Supports PNG, JPEG, GIF (87a/89a), and WEBP (RIFF / WEBP
/// container). Returns `None` when the prefix does not match a known
/// image format. The exhaustive variant list matches what the upstream
/// providers (Anthropic / OpenAI / Google / Bedrock) accept for inline
/// image content blocks; everything else has to round-trip as text.
pub fn infer_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

impl LlmRequest {
    /// Refuse to ship a request that carries `LlmInputItem::Image`
    /// payloads when the destination model's
    /// [`crate::ModelCapabilities::vision`] flag is false. Each provider's
    /// `stream_response` calls this before building the wire body so the
    /// caller sees a structured error (`SqueezyError::ProviderRequest`)
    /// instead of an upstream-rejected 4xx with a vendor-specific
    /// message. Models that are unknown to the registry (custom presets,
    /// fresh aggregator SKUs) fall back to the conservative
    /// `vision: false` default and surface the same error — callers can
    /// extend `models.json` or attach `model_discovery::ResolvedCapabilities`
    /// to opt in.
    pub fn ensure_vision_support(&self, provider: &str) -> Result<()> {
        if !self.input.iter().any(LlmInputItem::is_image) {
            return Ok(());
        }
        let supports_vision =
            crate::capabilities_for(provider, &self.model).is_some_and(|caps| caps.vision);
        if supports_vision {
            return Ok(());
        }
        Err(SqueezyError::ProviderRequest(format!(
            "model `{model}` on provider `{provider}` does not support image inputs (capabilities.vision = false); pick a vision-capable model before attaching an image",
            model = self.model,
        )))
    }
}

/// Normalize tool-call IDs across a replay sequence so providers see
/// a consistent canonical form (`call_1`, `call_2`, …) and every
/// `FunctionCallOutput` has a matching `FunctionCall` preceding it.
///
/// Mid-session model switches (Anthropic ↔ OpenAI ↔ Google ↔
/// Bedrock ↔ Ollama) leave a mixed pile of provider-specific
/// tool-call ids in the persisted `LlmInputItem` stream:
///
/// - Anthropic emits `toolu_…` and requires `^[a-zA-Z0-9_-]+$` with
///   ≤ 64 chars.
/// - OpenAI Responses emits 450+-char base64-like ids containing `|`
///   that Anthropic rejects outright.
/// - Google emits `google_call_N`, Ollama emits `ollama_call_N`,
///   Bedrock emits `tooluse_…`.
/// - Chat-Completions aggregators echo whatever the upstream sent.
///
/// Replaying this stream to a different provider without
/// normalization either fails the destination's id-shape check or
/// fails to match a tool result to its tool call. This pass:
///
/// 1. Walks `items` in order and assigns each distinct original
///    `call_id` a canonical `call_<N>` id (1-indexed by first
///    occurrence), rewriting both `FunctionCall` and
///    `FunctionCallOutput` items so the pair stays linked.
/// 2. When a `FunctionCallOutput` appears whose `call_id` was not
///    introduced by a prior `FunctionCall` in the same slice (orphan
///    tool result — common after a model switch discarded the
///    assistant turn that called the tool), synthesizes a placeholder
///    `FunctionCall` with `name = MODEL_SWITCHED_PLACEHOLDER_NAME`
///    and `arguments = {"reason": "model_switched"}` *before* the
///    output so the wire format stays well-formed.
///
/// Non-tool items (`UserText`, `AssistantText`, `Reasoning`) pass
/// through unchanged. Each provider's `request_body` calls this once
/// at the front of its existing transform so the per-provider wire
/// shapes are preserved.
pub(crate) fn normalize_tool_ids_for_replay(items: &[LlmInputItem]) -> Vec<LlmInputItem> {
    let mut id_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut next_index: usize = 0;
    let mut out: Vec<LlmInputItem> = Vec::with_capacity(items.len());

    for item in items {
        match item {
            LlmInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                let canonical = canonicalize_call_id(call_id, &mut id_map, &mut next_index);
                out.push(LlmInputItem::FunctionCall {
                    call_id: canonical,
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
            }
            LlmInputItem::FunctionCallOutput {
                call_id,
                output,
                content_parts,
                is_error,
            } => {
                let already_seen = id_map.contains_key(call_id);
                let canonical = canonicalize_call_id(call_id, &mut id_map, &mut next_index);
                if !already_seen {
                    // Orphan tool result: synthesize a placeholder
                    // FunctionCall in front of it so the destination
                    // provider has a matching tool_use to pair with
                    // this output. Without this Anthropic / Bedrock
                    // reject the request with a "tool_result without
                    // matching tool_use" error and the OpenAI
                    // Responses path returns a similar shape error.
                    out.push(LlmInputItem::FunctionCall {
                        call_id: canonical.clone(),
                        name: MODEL_SWITCHED_PLACEHOLDER_NAME.to_string(),
                        arguments: serde_json::json!({ "reason": "model_switched" }),
                    });
                }
                out.push(LlmInputItem::FunctionCallOutput {
                    call_id: canonical,
                    output: output.clone(),
                    content_parts: content_parts.clone(),
                    is_error: *is_error,
                });
            }
            other => out.push(other.clone()),
        }
    }

    out
}

fn canonicalize_call_id(
    original: &str,
    id_map: &mut std::collections::HashMap<String, String>,
    next_index: &mut usize,
) -> String {
    if let Some(canonical) = id_map.get(original) {
        return canonical.clone();
    }
    *next_index += 1;
    let canonical = format!("call_{}", *next_index);
    id_map.insert(original.to_string(), canonical.clone());
    canonical
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub strict: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}

/// Normalized completion cause. Each provider maps its native `stop_reason`
/// (Anthropic), `finish_reason`/`incomplete_details.reason` (OpenAI),
/// `finishReason` (Google), Bedrock `stopReason`, or Ollama `done_reason`
/// into one of these variants so the agent can branch on a single shape.
///
/// `EndTurn` is the model voluntarily releasing the turn; `ToolUse` means
/// the model wants to invoke tools; `MaxTokens` and `ContextWindowExceeded`
/// are truncation signals the agent surfaces explicitly so the user (and
/// future compaction-retry logic) can act on them instead of seeing a bare
/// provider error; `StopSequence` and `Refusal` carry the remaining
/// semantically distinct cases; `Other` keeps provider-specific strings
/// reachable without forcing the registry to enumerate every value.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    ContextWindowExceeded,
    StopSequence,
    Refusal,
    /// Anthropic Messages API `pause_turn`: the model voluntarily
    /// paused mid-turn (typically when a hosted tool is still
    /// processing) and expects the caller to re-issue the request
    /// with the partial state. Distinct from `EndTurn` so the agent
    /// can branch the recovery path.
    PauseTurn,
    /// Google Gemini `MALFORMED_FUNCTION_CALL`: the model emitted a
    /// tool call whose arguments JSON the upstream parser rejected.
    /// Distinct from `Refusal` (safety) and `Other` so the agent can
    /// retry with stricter `tool_choice` shaping.
    MalformedFunctionCall,
    Other(String),
}

impl StopReason {
    /// Parse Anthropic Messages API `stop_reason` strings.
    pub fn from_anthropic(value: &str) -> Self {
        match value {
            "end_turn" => Self::EndTurn,
            "tool_use" => Self::ToolUse,
            "max_tokens" => Self::MaxTokens,
            "model_context_window_exceeded" => Self::ContextWindowExceeded,
            "stop_sequence" => Self::StopSequence,
            "refusal" => Self::Refusal,
            "pause_turn" => Self::PauseTurn,
            other => Self::Other(other.to_string()),
        }
    }

    /// Parse OpenAI Responses API `incomplete_details.reason` strings.
    pub fn from_openai_incomplete(value: &str) -> Self {
        match value {
            "max_output_tokens" => Self::MaxTokens,
            "content_filter" => Self::Refusal,
            other => Self::Other(other.to_string()),
        }
    }

    /// Parse Google `candidates[0].finishReason` strings.
    pub fn from_google(value: &str) -> Self {
        match value {
            "STOP" => Self::EndTurn,
            "MAX_TOKENS" => Self::MaxTokens,
            "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY"
            | "LANGUAGE" | "RECITATION" => Self::Refusal,
            "MALFORMED_FUNCTION_CALL" => Self::MalformedFunctionCall,
            other => Self::Other(other.to_string()),
        }
    }

    /// Parse Bedrock Converse `stopReason` strings.
    pub fn from_bedrock(value: &str) -> Self {
        match value {
            "end_turn" => Self::EndTurn,
            "tool_use" => Self::ToolUse,
            "max_tokens" => Self::MaxTokens,
            "model_context_window_exceeded" => Self::ContextWindowExceeded,
            "stop_sequence" => Self::StopSequence,
            "guardrail_intervened" | "content_filtered" => Self::Refusal,
            other => Self::Other(other.to_string()),
        }
    }

    /// Parse Ollama `done_reason` strings.
    pub fn from_ollama(value: &str) -> Self {
        match value {
            "stop" => Self::EndTurn,
            "length" => Self::MaxTokens,
            other => Self::Other(other.to_string()),
        }
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum LlmEvent {
    Started,
    TextDelta(String),
    ReasoningDelta {
        text: String,
        kind: ReasoningKind,
    },
    ReasoningDone(ReasoningPayload),
    ToolCall(LlmToolCall),
    /// Incremental tool-arguments delta. Some providers stream tool
    /// arguments token-by-token before the full call materializes:
    /// OpenAI Responses' `response.function_call_arguments.delta`
    /// (H-07) and Anthropic-style incremental tool-args. Emitted
    /// additively while the call buffers; downstream consumers may
    /// display a progressive "calling tool …" hint or ignore it. The
    /// canonical `ToolCall` event still fires once the full call is
    /// known, so consumers that only care about the materialized call
    /// can wildcard-skip this variant.
    ToolCallDelta {
        call_id: String,
        name: String,
        arguments_chunk: String,
    },
    /// OpenAI safety-refusal text delta. The Responses API emits a
    /// dedicated `response.refusal.delta` stream when the model
    /// declines to answer; surfacing the running text lets the TUI
    /// show the refusal verbatim. The terminal `Completed` event
    /// still carries `stop_reason: Refusal` for the canonical signal.
    /// Consumers that only care about the canonical event stream can
    /// wildcard-skip this variant.
    Refusal {
        content: String,
    },
    /// Triple-path overflow detector classified this turn's terminal
    /// shape as a context-window overflow. See
    /// [`crate::overflow::classify_terminal`] for the three shapes the
    /// signal can carry. Emitted additively right before
    /// [`LlmEvent::Completed`] (or the provider's terminal error) so
    /// consumers that only care about the canonical event stream can
    /// ignore it. The agent's turn loop reacts to the signal once —
    /// compact, surface the upstream message, or stop the loop —
    /// instead of replaying the same overflowing call.
    ContextOverflow {
        provider: String,
        signal: OverflowSignal,
    },
    /// Server-echoed model id differed from the model the request
    /// asked for. Emitted at most once per stream, additively, right
    /// after [`LlmEvent::Started`], when the provider's first response
    /// chunk carries a `model` field that does not match
    /// `LlmRequest::model`. Surfaces silent provider-side fallback
    /// (Anthropic regional substitution, OpenAI snapshot pinning,
    /// aggregator routing rewrites, Ollama base-tag canonicalization,
    /// Google `modelVersion` pinning, etc.) so the agent / TUI /
    /// transcript record the actual model that produced the turn
    /// instead of just the one the user asked for. Consumers that
    /// only care about the canonical event stream can ignore it.
    ServerModel(String),
    Completed {
        response_id: Option<String>,
        cost: CostSnapshot,
        /// Normalized completion cause. `None` when the provider stream
        /// closed without emitting one (e.g. transport truncation handled
        /// elsewhere). Producers that have a native value MUST populate
        /// this; the agent uses it to drive explicit recovery branches.
        stop_reason: Option<StopReason>,
        /// `true` iff the stream finished with `stop_reason=EndTurn`,
        /// no content or tool-call delta latched
        /// `state.saw_visible_output`, AND the reasoning buffer was
        /// non-empty.
        ///
        /// This is the canonical Qwen3 / DeepSeek-R1 "reasoning-only
        /// finish" pattern — model thinks, model stops, no actionable
        /// output. Agent loop consumers may retry the turn once when
        /// this flag is set. Separate from `stop_reason` because the
        /// normalized `EndTurn` variant alone can't distinguish a clean
        /// "model emitted a real answer and stopped" from a degenerate
        /// "model spent the round on reasoning and stopped with
        /// nothing visible".
        #[serde(default)]
        reasoning_only_stop: bool,
    },
    Cancelled,
}

/// Once-per-stream tracker for the [`LlmEvent::ServerModel`] echo.
///
/// Every provider stream handler that observes a server-echoed model id
/// (Anthropic `message_start.message.model`, OpenAI Responses
/// `response.model`, Chat-Completions top-level `model`, Google
/// `modelVersion`, Ollama `model`) feeds it to [`Self::observe`]. The
/// helper returns `Some(LlmEvent::ServerModel(...))` exactly once per
/// stream — the first time the server's echo differs from the
/// requested model — and `None` thereafter (including when the echo
/// matches the request, which is the steady-state happy path and
/// should stay off the event stream entirely).
#[derive(Debug, Default)]
pub(crate) struct ServerModelEcho {
    emitted: bool,
}

impl ServerModelEcho {
    /// Compare the server-echoed model against the requested model and
    /// return `Some(LlmEvent::ServerModel(server))` on the first
    /// observation of a mismatch. Subsequent calls return `None`. An
    /// empty `server` string is ignored (treated as "echo missing").
    /// Both arguments are compared verbatim — providers do not
    /// canonicalize aliases or versioned snapshot ids here, so callers
    /// see the exact strings the upstream chose to send back.
    pub(crate) fn observe(&mut self, requested: &str, server: &str) -> Option<LlmEvent> {
        if self.emitted {
            return None;
        }
        if server.is_empty() {
            return None;
        }
        // Match seals the tracker so a later (identical) echo on the
        // same stream cannot accidentally re-emit if the upstream
        // duplicates the field across chunks.
        self.emitted = true;
        if server == requested {
            return None;
        }
        Some(LlmEvent::ServerModel(server.to_string()))
    }
}

impl LlmEvent {
    /// Construct a `Completed` event with no provider-reported stop
    /// reason and no reasoning-only-stop marker. Convenience for test
    /// code and synthetic completions (replay reconstruction, helper
    /// turn paths) that don't carry a real upstream signal.
    pub fn completed(response_id: Option<String>, cost: CostSnapshot) -> Self {
        LlmEvent::Completed {
            response_id,
            cost,
            stop_reason: None,
            reasoning_only_stop: false,
        }
    }

    /// Construct a `Completed` event with explicit normalized
    /// `stop_reason` and `reasoning_only_stop` markers. Used by the
    /// Chat-Completions provider when the upstream surfaces a real
    /// terminal reason AND we want the reasoning-only-stop signal
    /// latched.
    pub fn completed_with_reason(
        response_id: Option<String>,
        cost: CostSnapshot,
        stop_reason: Option<StopReason>,
        reasoning_only_stop: bool,
    ) -> Self {
        LlmEvent::Completed {
            response_id,
            cost,
            stop_reason,
            reasoning_only_stop,
        }
    }
}

pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream;
}

#[derive(Debug, Clone)]
pub struct UnavailableProvider {
    name: &'static str,
    reason: Arc<str>,
}

impl UnavailableProvider {
    pub fn new(name: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name,
            reason: Arc::from(reason.into()),
        }
    }
}

impl LlmProvider for UnavailableProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn stream_response(&self, _request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        let reason = self.reason.clone();
        Box::pin(futures_util::stream::once(async move {
            Err(SqueezyError::ProviderNotConfigured(reason.to_string()))
        }))
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
