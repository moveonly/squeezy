use std::{
    collections::HashMap,
    io::{self, Write},
    sync::{Arc, LazyLock, RwLock},
};

use serde::Deserialize;
use serde_json::Value;
use squeezy_core::{
    CostSnapshot, ModelProfile, OpenAiCompatiblePreset, ProviderConfig, ReasoningEffort, Result,
};

use crate::{
    AnthropicProvider, BedrockProvider, GoogleProvider, LlmInputItem, LlmProvider, LlmRequest,
    OllamaProvider, OpenAiCodexProvider, OpenAiCompatibleProvider, OpenAiProvider, XaiProvider,
    limits::{ContextLimitInput, LimitConfidence, LimitSource, resolve_context_limits},
    oauth::GitHubCopilotProvider,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct ModelCapabilities {
    pub streaming: bool,
    pub tool_calling: bool,
    pub json_mode: bool,
    pub vision: bool,
    pub response_state: bool,
    pub reasoning_tokens: bool,
    pub reasoning_effort: bool,
    pub text_verbosity: bool,
    #[serde(default)]
    pub prompt_caching: bool,
    /// Default `reasoning_effort` the provider should apply when the
    /// caller leaves [`LlmRequest::reasoning_effort`] unset. `None`
    /// preserves the historical behavior of "let the provider pick";
    /// `Some(...)` lets the Phase 3 catalog encode model-recommended
    /// defaults (e.g. GPT-5 high vs medium baseline). Per-provider
    /// consumption lands in Phase 4.
    #[serde(default)]
    pub default_reasoning_effort: Option<ReasoningEffort>,
    /// Minimum thinking budget the provider supports for the model.
    /// `None` leaves the provider default in place. Per-provider
    /// consumption lands in Phase 4.
    #[serde(default)]
    pub thinking_budget_min: Option<u32>,
    /// Maximum thinking budget the provider supports for the model.
    /// `None` leaves the provider default in place. Per-provider
    /// consumption lands in Phase 4.
    #[serde(default)]
    pub thinking_budget_max: Option<u32>,
}

impl ModelCapabilities {
    pub const TEXT_TOOLS: Self = Self {
        streaming: true,
        tool_calling: true,
        json_mode: true,
        vision: false,
        response_state: false,
        reasoning_tokens: false,
        reasoning_effort: false,
        text_verbosity: false,
        prompt_caching: false,
        default_reasoning_effort: None,
        thinking_budget_min: None,
        thinking_budget_max: None,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct TokenPricing {
    pub input_usd_micros_per_mtok: u64,
    pub output_usd_micros_per_mtok: u64,
    pub cache_read_usd_micros_per_mtok: Option<u64>,
    pub cache_write_usd_micros_per_mtok: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenizerKind {
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
    Anthropic,
    Google,
    Ollama,
}

impl TokenizerKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai_compatible_estimate",
            Self::Anthropic => "anthropic_estimate",
            Self::Google => "google_estimate",
            Self::Ollama => "ollama_estimate",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelLifecycle {
    Active,
    Local,
}

impl ModelLifecycle {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Local => "local",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct ModelLimits {
    pub context_window_tokens: u64,
    pub max_output_tokens: u64,
    #[serde(default = "default_effective_context_window_percent")]
    pub effective_context_window_percent: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelInfo {
    pub provider: &'static str,
    pub id: &'static str,
    pub profile: ModelProfile,
    pub capabilities: ModelCapabilities,
    pub pricing: Option<TokenPricing>,
    pub limits: Option<ModelLimits>,
    pub tokenizer: TokenizerKind,
    pub lifecycle: ModelLifecycle,
    pub metadata_source: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestTokenEstimate {
    pub input_tokens: u64,
    pub context_window_tokens: Option<u64>,
    pub effective_context_window_tokens: Option<u64>,
    pub headroom_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub input_budget_tokens: Option<u64>,
    pub remaining_input_tokens: Option<u64>,
    /// Hundredths of one percent. `10_000` means 100.00%.
    pub used_input_percent_x100: Option<u32>,
    pub tokenizer: TokenizerKind,
    pub estimated: bool,
    /// Where `context_window_tokens` came from, and how much to trust it.
    pub limit_source: LimitSource,
    pub limit_confidence: LimitConfidence,
    /// Set when a provider context-overflow error clamped the window.
    pub observed_ceiling_tokens: Option<u64>,
    /// The window models.dev reports (even if not the selected source).
    pub models_dev_window_tokens: Option<u64>,
    /// The percent + flat reserve used to derive `effective_context_window_tokens`,
    /// surfaced so the previously-hidden reduction is inspectable.
    pub effective_context_window_percent: u8,
    pub baseline_reserve_tokens: u64,
}

pub static MODEL_REGISTRY: LazyLock<Vec<ModelInfo>> = LazyLock::new(load_models);
static FALLBACK_MODEL_CACHE: LazyLock<RwLock<FallbackModelCache>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

type FallbackModelCache = HashMap<&'static str, HashMap<&'static str, &'static ModelInfo>>;

#[derive(Debug, Deserialize)]
struct RawModelInfo {
    provider: String,
    id: String,
    profile: ModelProfile,
    capabilities: ModelCapabilities,
    pricing: Option<TokenPricing>,
    limits: Option<ModelLimits>,
    tokenizer: TokenizerKind,
    lifecycle: ModelLifecycle,
    metadata_source: String,
}

pub(crate) fn default_effective_context_window_percent() -> u8 {
    95
}

/// Flat token reserve carved off every effective window for system framing
/// (tool schemas, instructions, response scaffolding) that the per-request
/// estimate can't see ahead of time. Exposed so the resolver and the UI share
/// one constant instead of a buried magic number.
pub const DEFAULT_BASELINE_RESERVE_TOKENS: u64 = 12_000;

fn load_models() -> Vec<ModelInfo> {
    let raw = serde_json::from_str::<Vec<RawModelInfo>>(include_str!("models.json"))
        .expect("bundled models.json must be valid");
    raw.into_iter().map(model_info_from_raw).collect()
}

fn model_info_from_raw(raw: RawModelInfo) -> ModelInfo {
    ModelInfo {
        provider: leak_string(raw.provider),
        id: leak_string(raw.id),
        profile: raw.profile,
        capabilities: raw.capabilities,
        pricing: raw.pricing,
        limits: raw.limits,
        tokenizer: raw.tokenizer,
        lifecycle: raw.lifecycle,
        metadata_source: leak_string(raw.metadata_source),
    }
}

fn fallback_model_info(provider: &'static str, model: &'static str) -> ModelInfo {
    let tokenizer = fallback_tokenizer(provider, model);
    let mut capabilities = ModelCapabilities::TEXT_TOOLS;
    capabilities.json_mode = false;
    ModelInfo {
        provider,
        id: model,
        profile: ModelProfile::Balanced,
        capabilities,
        pricing: None,
        limits: Some(ModelLimits {
            context_window_tokens: 272_000,
            max_output_tokens: 64_000,
            effective_context_window_percent: 95,
        }),
        tokenizer,
        lifecycle: if provider == "ollama" {
            ModelLifecycle::Local
        } else {
            ModelLifecycle::Active
        },
        metadata_source: "fallback",
    }
}

fn cached_fallback_model_info(provider: &str, model: &str) -> &'static ModelInfo {
    {
        let guard = FALLBACK_MODEL_CACHE
            .read()
            .expect("fallback model cache lock poisoned");
        if let Some(entry) = guard
            .get(provider)
            .and_then(|models| models.get(model))
            .copied()
        {
            return entry;
        }
    }

    let mut guard = FALLBACK_MODEL_CACHE
        .write()
        .expect("fallback model cache lock poisoned");
    if let Some(entry) = guard
        .get(provider)
        .and_then(|models| models.get(model))
        .copied()
    {
        return entry;
    }

    let provider = leak_string(provider.to_string());
    let model = leak_string(model.to_string());
    let leaked: &'static mut ModelInfo = Box::leak(Box::new(fallback_model_info(provider, model)));
    let entry: &'static ModelInfo = &*leaked;
    guard.entry(provider).or_default().insert(model, entry);
    entry
}

/// Best-guess tokenizer for `(provider, model)` pairs that aren't in the
/// curated registry. The native single-vendor providers map directly; OpenAI-
/// compatible aggregators expose vendor-namespaced model ids
/// (`anthropic/claude-opus-4-7`, `google/gemini-2.5-pro`), and we honour the
/// namespace so token estimates use the vendor's actual ratio.
fn fallback_tokenizer(provider: &str, model: &str) -> TokenizerKind {
    match provider {
        "anthropic" | "bedrock" => TokenizerKind::Anthropic,
        "google" => TokenizerKind::Google,
        "ollama" => TokenizerKind::Ollama,
        _ => {
            let lower = model.to_ascii_lowercase();
            let id = lower.split_once('/').map(|(_, id)| id).unwrap_or(&lower);
            if id.starts_with("claude") {
                TokenizerKind::Anthropic
            } else if id.starts_with("gemini") {
                TokenizerKind::Google
            } else {
                TokenizerKind::OpenAiCompatible
            }
        }
    }
}

fn leak_string(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

pub const PROVIDERS: &[&str] = &[
    "openai",
    "openai_codex",
    "github_copilot",
    "anthropic",
    "google",
    "azure_openai",
    "bedrock",
    "ollama",
    "openrouter",
    "vercel",
    "portkey",
    "groq",
    "xai",
    "deepseek",
    "vertex",
    "mistral",
    "together",
    "fireworks",
    "cerebras",
    "deepinfra",
    "baseten",
    "lmstudio",
    "vllm",
    "llamacpp",
    "cloudflare_workers_ai",
    "cloudflare_ai_gateway",
    "openai_compatible",
];

pub fn models_for_provider(provider: &str) -> impl Iterator<Item = &'static ModelInfo> {
    MODEL_REGISTRY
        .iter()
        .filter(move |model| model.provider == provider)
        .filter(|model| is_text_model_picker_eligible(model.provider, model.id))
}

fn is_text_model_picker_eligible(provider: &str, model: &str) -> bool {
    if provider != "xai" {
        return true;
    }
    !model
        .rsplit_once('/')
        .map(|(_, id)| id)
        .unwrap_or(model)
        .to_ascii_lowercase()
        .starts_with("grok-imagine")
}

pub fn model_info_for(provider: &str, model: &str) -> Option<&'static ModelInfo> {
    curated_model_info_for(provider, model)
        .or_else(|| Some(cached_fallback_model_info(provider, model)))
}

/// Curated registry lookup WITHOUT the synthetic-fallback tail. Returns `None`
/// for any `(provider, model)` not hand-curated in `models.json`, so the limit
/// resolver can distinguish a real bundled entry (trustworthy) from the 272K
/// guess and let models.dev fill the gap in between.
pub fn curated_model_info_for(provider: &str, model: &str) -> Option<&'static ModelInfo> {
    MODEL_REGISTRY
        .iter()
        .find(|entry| entry.provider == provider && entry.id == model)
}

pub fn capabilities_for(provider: &str, model: &str) -> Option<ModelCapabilities> {
    model_info_for(provider, model).map(|entry| entry.capabilities)
}

/// Providers whose wire path does NOT forward [`LlmRequest::output_schema`]
/// to the server: Anthropic and Bedrock have no structured-output field on
/// their request bodies, and Ollama's native chat route drops it. Every
/// other provider — the OpenAI Responses family (`openai`, `openai_codex`,
/// `azure_openai`), Google (`responseSchema`), xAI, and the OpenAI-compatible
/// presets (`response_format`) — emits the schema. Callers that attach a
/// strict schema must skip it for these so the model is asked for the same
/// free-form output it gets today and the loose parser stays the contract.
const OUTPUT_SCHEMA_OBLIVIOUS_PROVIDERS: &[&str] = &["anthropic", "bedrock", "ollama"];

/// True when `provider`/`model` honors a strict JSON [`LlmRequest::output_schema`]
/// on the wire — i.e. the provider forwards the schema AND the model advertises
/// JSON-mode support. Returns `false` (preserving the historical free-form
/// request) for providers that silently drop the schema or models without
/// JSON support, so attaching a schema is always a safe no-op there.
pub fn provider_honors_output_schema(provider: &str, model: &str) -> bool {
    if OUTPUT_SCHEMA_OBLIVIOUS_PROVIDERS.contains(&provider) {
        return false;
    }
    capabilities_for(provider, model).is_some_and(|caps| caps.json_mode)
}

pub fn estimate_request_context(
    provider: &str,
    model: &str,
    request: &LlmRequest,
    context_window_override: Option<u64>,
) -> RequestTokenEstimate {
    estimate_request_context_calibrated(provider, model, request, context_window_override, None)
}

/// Variant of [`estimate_request_context`] that uses a caller-supplied
/// [`TokenCalibration`] to convert bytes to tokens. When `calibration` is
/// `None` we fall back to the provider's default bytes-per-token ratio. Does
/// NOT consult the models.dev cache (so unit tests stay deterministic); use
/// [`estimate_request_context_full`] to thread the live catalog and the other
/// resolver layers.
pub fn estimate_request_context_calibrated(
    provider: &str,
    model: &str,
    request: &LlmRequest,
    context_window_override: Option<u64>,
    calibration: Option<&crate::tokens::TokenCalibration>,
) -> RequestTokenEstimate {
    let mut input = ContextLimitInput::new(provider, model);
    input.user_override = context_window_override;
    estimate_request_context_full(&input, request, calibration)
}

/// Full estimate driven by the layered limit resolver. Callers supply whatever
/// resolution layers they have (override, live provider window, observed
/// ceiling, models.dev view, effective-window knobs) via [`ContextLimitInput`];
/// the provenance of the chosen window rides back out on the estimate.
pub fn estimate_request_context_full(
    input: &ContextLimitInput<'_>,
    request: &LlmRequest,
    calibration: Option<&crate::tokens::TokenCalibration>,
) -> RequestTokenEstimate {
    let provider = input.provider;
    let model = input.model;
    let bytes_per_token = calibration
        .map(|c| c.bytes_per_token(provider))
        .unwrap_or_else(|| crate::tokens::default_bytes_per_token(provider));
    let tokenizer = curated_model_info_for(provider, model)
        .map(|entry| entry.tokenizer)
        .unwrap_or_else(|| fallback_tokenizer(provider, model));
    let input_tokens = estimate_request_input_tokens(request, bytes_per_token);

    let resolved = resolve_context_limits(input);
    let context_window_tokens = resolved.context_window_tokens;
    let effective_context_window_tokens = crate::limits::effective_window_tokens(&resolved);
    let headroom_tokens = context_window_tokens
        .zip(effective_context_window_tokens)
        .map(|(raw_window, effective_window)| raw_window.saturating_sub(effective_window));
    // Cap an explicit request value at the model's real max output, but NOT at
    // the synthetic 64K fallback for unknown models — that would silently
    // shrink what the operator asked for. The cap only applies when the limit
    // came from a trustworthy layer.
    let max_output_cap = if matches!(resolved.source, LimitSource::SyntheticFallback) {
        None
    } else {
        resolved.max_output_tokens
    };
    let max_output_tokens = request
        .max_output_tokens
        .map(u64::from)
        .or(resolved.max_output_tokens)
        .map(|tokens| max_output_cap.map(|cap| tokens.min(cap)).unwrap_or(tokens));
    let input_budget_tokens = effective_context_window_tokens
        .map(|window| window.saturating_sub(max_output_tokens.unwrap_or(0)));
    let remaining_input_tokens =
        input_budget_tokens.map(|budget| budget.saturating_sub(input_tokens));
    let used_input_percent_x100 = input_budget_tokens
        .filter(|budget| *budget > 0)
        .map(|budget| {
            ((input_tokens.saturating_mul(10_000)) / budget).min(u64::from(u32::MAX)) as u32
        });
    RequestTokenEstimate {
        input_tokens,
        context_window_tokens,
        effective_context_window_tokens,
        headroom_tokens,
        max_output_tokens,
        input_budget_tokens,
        remaining_input_tokens,
        used_input_percent_x100,
        tokenizer,
        estimated: true,
        limit_source: resolved.source,
        limit_confidence: resolved.confidence,
        observed_ceiling_tokens: resolved.observed_ceiling_tokens,
        models_dev_window_tokens: resolved.models_dev_window_tokens,
        effective_context_window_percent: resolved.effective_context_window_percent,
        baseline_reserve_tokens: resolved.baseline_reserve_tokens,
    }
}

pub fn provider_name(config: &ProviderConfig) -> &'static str {
    match config {
        ProviderConfig::OpenAi(_) => "openai",
        ProviderConfig::Anthropic(_) => "anthropic",
        ProviderConfig::Google(_) => "google",
        ProviderConfig::AzureOpenAi(_) => "azure_openai",
        ProviderConfig::Bedrock(_) => "bedrock",
        ProviderConfig::Ollama(_) => "ollama",
        ProviderConfig::OpenAiCodex(_) => "openai_codex",
        ProviderConfig::GitHubCopilot(_) => "github_copilot",
        ProviderConfig::OpenAiCompatible(config) => config.preset.as_str(),
    }
}

pub fn provider_from_config(config: &ProviderConfig) -> Result<Arc<dyn LlmProvider>> {
    match config {
        ProviderConfig::OpenAi(openai) => Ok(Arc::new(OpenAiProvider::from_config(openai)?)),
        ProviderConfig::Anthropic(anthropic) => {
            Ok(Arc::new(AnthropicProvider::from_config(anthropic)?))
        }
        ProviderConfig::Google(google) => Ok(Arc::new(GoogleProvider::from_config(google)?)),
        ProviderConfig::AzureOpenAi(azure) => {
            Ok(Arc::new(OpenAiProvider::from_azure_config(azure)?))
        }
        ProviderConfig::Bedrock(bedrock) => Ok(Arc::new(BedrockProvider::from_config(bedrock)?)),
        ProviderConfig::Ollama(ollama) => Ok(Arc::new(OllamaProvider::from_config(ollama))),
        // `from_config` consults the on-disk token under
        // `~/.squeezy/auth/openai-codex.json`. Local file I/O only —
        // refresh happens lazily on the first streaming request
        // through the OAuth source.
        ProviderConfig::OpenAiCodex(codex) => {
            Ok(Arc::new(OpenAiCodexProvider::from_config(codex)?))
        }
        ProviderConfig::GitHubCopilot(config) => Ok(Arc::new(
            GitHubCopilotProvider::from_default_auth(config.transport)?,
        )),
        ProviderConfig::OpenAiCompatible(config) => match config.preset {
            // xAI ships both Chat Completions and Responses APIs on the
            // same host; route Grok 3+ through Responses for reasoning
            // summaries, drop older Grok models onto Chat.
            OpenAiCompatiblePreset::XAi => Ok(Arc::new(XaiProvider::from_config(config)?)),
            _ => Ok(Arc::new(OpenAiCompatibleProvider::from_config(config)?)),
        },
    }
}

pub fn estimate_cost(provider: &str, model: &str, cost: &CostSnapshot) -> Option<u64> {
    let pricing = model_info_for(provider, model).and_then(|entry| entry.pricing)?;
    let cached_input_tokens = cost.cached_input_tokens.unwrap_or(0);
    let cache_write_input_tokens = cost.cache_write_input_tokens.unwrap_or(0);
    // `CostSnapshot::input_tokens` is normalised to the cross-provider
    // convention: total prompt the model saw, including the cached and
    // cache-write shares. Anthropic / Bedrock providers do that fold-in
    // at the snapshot boundary (see `AnthropicStreamState::cost`,
    // `BedrockStreamState::cost`), so the standard-rate share is
    // always the remainder after subtracting cached and cache-write.
    let standard_input_tokens = cost
        .input_tokens
        .unwrap_or(0)
        .saturating_sub(cached_input_tokens)
        .saturating_sub(cache_write_input_tokens);
    Some(
        estimate_tokens(standard_input_tokens, pricing.input_usd_micros_per_mtok)
            + estimate(cost.output_tokens, pricing.output_usd_micros_per_mtok)
            + estimate_tokens(
                cached_input_tokens,
                pricing.cache_read_usd_micros_per_mtok.unwrap_or(0),
            )
            + estimate_tokens(
                cache_write_input_tokens,
                pricing.cache_write_usd_micros_per_mtok.unwrap_or(0),
            ),
    )
}

fn estimate(tokens: Option<u64>, usd_micros_per_mtok: u64) -> u64 {
    estimate_tokens(tokens.unwrap_or(0), usd_micros_per_mtok)
}

fn estimate_tokens(tokens: u64, usd_micros_per_mtok: u64) -> u64 {
    tokens.saturating_mul(usd_micros_per_mtok) / 1_000_000
}

fn estimate_request_input_tokens(request: &LlmRequest, bytes_per_token: f64) -> u64 {
    let mut total = estimate_text_tokens(&request.instructions, bytes_per_token).saturating_add(8);
    if let Some(verbosity) = request.response_verbosity {
        total = total.saturating_add(estimate_text_tokens(verbosity.as_str(), bytes_per_token) + 4);
    }
    if let Some(effort) = request.reasoning_effort {
        total = total.saturating_add(estimate_text_tokens(effort.as_str(), bytes_per_token) + 4);
    }
    if request.previous_response_id.is_some() {
        total = total.saturating_add(8);
    }
    if request.store {
        total = total.saturating_add(2);
    }
    for item in request.input.iter() {
        total = total.saturating_add(estimate_input_item_tokens(item, bytes_per_token));
    }
    for tool in request.tools.iter() {
        total = total.saturating_add(12);
        total = total.saturating_add(estimate_text_tokens(&tool.name, bytes_per_token));
        total = total.saturating_add(estimate_text_tokens(&tool.description, bytes_per_token));
        total = total.saturating_add(estimate_json_tokens(&tool.parameters, bytes_per_token));
    }
    total
}

fn estimate_input_item_tokens(item: &LlmInputItem, bytes_per_token: f64) -> u64 {
    match item {
        LlmInputItem::UserText(text) | LlmInputItem::AssistantText(text) => {
            estimate_text_tokens(text, bytes_per_token).saturating_add(8)
        }
        LlmInputItem::FunctionCall {
            call_id,
            name,
            arguments,
        } => estimate_text_tokens(call_id, bytes_per_token)
            .saturating_add(estimate_text_tokens(name, bytes_per_token))
            .saturating_add(estimate_json_tokens(arguments, bytes_per_token))
            .saturating_add(12),
        LlmInputItem::FunctionCallOutput {
            call_id, output, ..
        } => estimate_text_tokens(call_id, bytes_per_token)
            .saturating_add(estimate_text_tokens(output, bytes_per_token))
            .saturating_add(12),
        LlmInputItem::Reasoning(payload) => {
            let text = payload.display_text();
            estimate_text_tokens(&text, bytes_per_token).saturating_add(8)
        }
        // Vision providers convert images to token-equivalent "tiles" on
        // the server side (Claude ≈ 1 tile per 750 image pixels, OpenAI
        // ≈ 85 base + 170 per high-detail tile, Gemini ≈ 258 per image).
        // Without per-provider tile geometry we approximate as a flat
        // 1024-token attachment plus the byte cost of the base64 wire
        // form — high enough that the agent budgets headroom for an
        // attached image, low enough that text-heavy turns are
        // unaffected.
        LlmInputItem::Image { bytes, .. } => {
            let wire_bytes = (bytes.len() as f64 * 4.0 / 3.0).ceil();
            let wire_tokens = (wire_bytes / bytes_per_token.max(0.1)).ceil() as u64;
            wire_tokens.saturating_add(1024)
        }
        // Documents lower to bytes on the wire too; charge the same
        // base64 overhead estimate as `Image`. Providers that don't
        // accept documents will drop the item at build time, but the
        // budget keeps headroom symmetric until then.
        LlmInputItem::Document { bytes, .. } => {
            let wire_bytes = (bytes.len() as f64 * 4.0 / 3.0).ceil();
            let wire_tokens = (wire_bytes / bytes_per_token.max(0.1)).ceil() as u64;
            wire_tokens.saturating_add(1024)
        }
    }
}

fn estimate_json_tokens(value: &Value, bytes_per_token: f64) -> u64 {
    let mut writer = CountingWriter::default();
    serde_json::to_writer(&mut writer, value)
        .map(|_| estimate_byte_tokens(writer.bytes, bytes_per_token))
        .unwrap_or(0)
}

/// Convert a UTF-8 text blob into an approximate token count using the given
/// bytes-per-token ratio. The ratio is provider-specific (and EMA-calibrated
/// when a `TokenCalibration` is in play) so calibrated callers see closer
/// estimates than the historical hard-coded `bytes / 4`.
fn estimate_text_tokens(text: &str, bytes_per_token: f64) -> u64 {
    estimate_byte_tokens(text.len(), bytes_per_token)
}

fn estimate_byte_tokens(bytes: usize, bytes_per_token: f64) -> u64 {
    if bytes == 0 {
        return 0;
    }
    let estimate = (bytes as f64 / bytes_per_token.max(0.1)).ceil() as u64;
    estimate.max(1)
}

#[derive(Debug, Default)]
struct CountingWriter {
    bytes: usize,
}

impl Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
