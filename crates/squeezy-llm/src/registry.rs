use std::sync::{Arc, LazyLock};

use serde::Deserialize;
use serde_json::Value;
use squeezy_core::{CostSnapshot, ModelProfile, OpenAiCompatiblePreset, ProviderConfig, Result};

use crate::{
    AnthropicProvider, BedrockProvider, FauxProvider, GoogleProvider, LlmInputItem, LlmProvider,
    LlmRequest, OllamaProvider, OpenAiCodexProvider, OpenAiCompatibleProvider, OpenAiProvider,
    XaiProvider,
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
}

pub static MODEL_REGISTRY: LazyLock<Vec<ModelInfo>> = LazyLock::new(load_models);

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

fn default_effective_context_window_percent() -> u8 {
    95
}

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

fn fallback_model_info(provider: &str, model: &str) -> ModelInfo {
    let tokenizer = fallback_tokenizer(provider, model);
    ModelInfo {
        provider: leak_string(provider.to_string()),
        id: leak_string(model.to_string()),
        profile: ModelProfile::Balanced,
        capabilities: ModelCapabilities::TEXT_TOOLS,
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
}

pub fn model_info_for(provider: &str, model: &str) -> Option<&'static ModelInfo> {
    MODEL_REGISTRY
        .iter()
        .find(|entry| entry.provider == provider && entry.id == model)
        .or_else(|| {
            let leaked: &'static mut ModelInfo =
                Box::leak(Box::new(fallback_model_info(provider, model)));
            Some(&*leaked)
        })
}

pub fn capabilities_for(provider: &str, model: &str) -> Option<ModelCapabilities> {
    model_info_for(provider, model).map(|entry| entry.capabilities)
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
/// `None` we fall back to the provider's default bytes-per-token ratio, so
/// the old behaviour is preserved exactly.
pub fn estimate_request_context_calibrated(
    provider: &str,
    model: &str,
    request: &LlmRequest,
    context_window_override: Option<u64>,
    calibration: Option<&crate::tokens::TokenCalibration>,
) -> RequestTokenEstimate {
    let bytes_per_token = calibration
        .map(|c| c.bytes_per_token(provider))
        .unwrap_or_else(|| crate::tokens::default_bytes_per_token(provider));
    let info = model_info_for(provider, model);
    let tokenizer = info
        .map(|entry| entry.tokenizer)
        .unwrap_or(TokenizerKind::OpenAiCompatible);
    let input_tokens = estimate_request_input_tokens(request, bytes_per_token);
    let model_limits = info.and_then(|entry| entry.limits);
    let context_window_tokens =
        context_window_override.or(model_limits.map(|limits| limits.context_window_tokens));
    let effective_context_window_tokens = context_window_tokens.map(|window| {
        let percent = model_limits
            .map(|limits| limits.effective_context_window_percent)
            .unwrap_or(95);
        window
            .saturating_mul(u64::from(percent))
            .saturating_div(100)
    });
    const BASELINE_TOKENS: u64 = 12_000;
    let effective_context_window_tokens =
        effective_context_window_tokens.map(|window| window.saturating_sub(BASELINE_TOKENS));
    let headroom_tokens = context_window_tokens
        .zip(effective_context_window_tokens)
        .map(|(raw_window, effective_window)| raw_window.saturating_sub(effective_window));
    let max_output_tokens = request
        .max_output_tokens
        .map(u64::from)
        .or(model_limits.map(|limits| limits.max_output_tokens))
        .map(|tokens| {
            model_limits
                .map(|limits| tokens.min(limits.max_output_tokens))
                .unwrap_or(tokens)
        });
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
        ProviderConfig::OpenAiCompatible(config) => config.preset.as_str(),
        ProviderConfig::Faux(_) => "faux",
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
        ProviderConfig::OpenAiCompatible(config) => match config.preset {
            // xAI ships both Chat Completions and Responses APIs on the
            // same host; route Grok 3+ through Responses for reasoning
            // summaries, drop older Grok models onto Chat.
            OpenAiCompatiblePreset::XAi => Ok(Arc::new(XaiProvider::from_config(config)?)),
            _ => Ok(Arc::new(OpenAiCompatibleProvider::from_config(config)?)),
        },
        // The faux provider runs entirely in-process. `from_config`
        // reads the configured script path (when set) and queues its
        // turns; programmatic callers can also build an empty provider
        // and `push_step` directly.
        ProviderConfig::Faux(config) => Ok(Arc::new(FauxProvider::from_config(config)?)),
    }
}

pub fn estimate_cost(provider: &str, model: &str, cost: &CostSnapshot) -> Option<u64> {
    let pricing = model_info_for(provider, model).and_then(|entry| entry.pricing)?;
    let cached_input_tokens = cost.cached_input_tokens.unwrap_or(0);
    let cache_write_input_tokens = cost.cache_write_input_tokens.unwrap_or(0);
    // OpenAI/Gemini report `input_tokens` as the total including cached and
    // cache-write tokens, so the standard share has to be derived by
    // subtraction. Anthropic's Messages API (and Bedrock's Claude variant)
    // report `input_tokens` as already-uncached, so the subtraction would
    // strip out tokens that aren't in the value and undercount the
    // standard-rate cost (often to zero when caching is active).
    let standard_input_tokens = match provider {
        "anthropic" | "bedrock" => cost.input_tokens.unwrap_or(0),
        _ => cost
            .input_tokens
            .unwrap_or(0)
            .saturating_sub(cached_input_tokens)
            .saturating_sub(cache_write_input_tokens),
    };
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
        LlmInputItem::FunctionCallOutput { call_id, output } => {
            estimate_text_tokens(call_id, bytes_per_token)
                .saturating_add(estimate_text_tokens(output, bytes_per_token))
                .saturating_add(12)
        }
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
    }
}

fn estimate_json_tokens(value: &Value, bytes_per_token: f64) -> u64 {
    serde_json::to_string(value)
        .map(|text| estimate_text_tokens(&text, bytes_per_token))
        .unwrap_or(0)
}

/// Convert a UTF-8 text blob into an approximate token count using the given
/// bytes-per-token ratio. The ratio is provider-specific (and EMA-calibrated
/// when a `TokenCalibration` is in play) so calibrated callers see closer
/// estimates than the historical hard-coded `bytes / 4`.
fn estimate_text_tokens(text: &str, bytes_per_token: f64) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let bytes = text.len() as f64;
    let estimate = (bytes / bytes_per_token.max(0.1)).ceil() as u64;
    estimate.max(1)
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
