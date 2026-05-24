use std::sync::Arc;

use squeezy_core::{CostSnapshot, ModelProfile, ProviderConfig, Result};

use crate::{
    AnthropicProvider, BedrockProvider, GoogleProvider, LlmProvider, OllamaProvider, OpenAiProvider,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub streaming: bool,
    pub tool_calling: bool,
    pub json_mode: bool,
    pub vision: bool,
    pub response_state: bool,
    pub reasoning_tokens: bool,
    pub reasoning_effort: bool,
    pub text_verbosity: bool,
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
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenPricing {
    pub input_usd_micros_per_mtok: u64,
    pub output_usd_micros_per_mtok: u64,
    pub cache_read_usd_micros_per_mtok: Option<u64>,
    pub cache_write_usd_micros_per_mtok: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelInfo {
    pub provider: &'static str,
    pub id: &'static str,
    pub profile: ModelProfile,
    pub capabilities: ModelCapabilities,
    pub pricing: Option<TokenPricing>,
}

pub const MODEL_REGISTRY: &[ModelInfo] = &[
    ModelInfo {
        provider: "openai",
        id: squeezy_core::DEFAULT_OPENAI_MODEL,
        profile: ModelProfile::Cheap,
        capabilities: ModelCapabilities {
            response_state: true,
            reasoning_tokens: true,
            reasoning_effort: true,
            text_verbosity: true,
            ..ModelCapabilities::TEXT_TOOLS
        },
        pricing: Some(TokenPricing {
            input_usd_micros_per_mtok: 50_000,
            output_usd_micros_per_mtok: 400_000,
            cache_read_usd_micros_per_mtok: Some(5_000),
            cache_write_usd_micros_per_mtok: None,
        }),
    },
    ModelInfo {
        provider: "anthropic",
        id: squeezy_core::DEFAULT_ANTHROPIC_MODEL,
        profile: ModelProfile::Cheap,
        capabilities: ModelCapabilities::TEXT_TOOLS,
        pricing: Some(TokenPricing {
            input_usd_micros_per_mtok: 800_000,
            output_usd_micros_per_mtok: 4_000_000,
            cache_read_usd_micros_per_mtok: Some(80_000),
            cache_write_usd_micros_per_mtok: Some(1_000_000),
        }),
    },
    ModelInfo {
        provider: "google",
        id: squeezy_core::DEFAULT_GOOGLE_MODEL,
        profile: ModelProfile::Cheap,
        capabilities: ModelCapabilities::TEXT_TOOLS,
        pricing: Some(TokenPricing {
            input_usd_micros_per_mtok: 100_000,
            output_usd_micros_per_mtok: 400_000,
            cache_read_usd_micros_per_mtok: Some(25_000),
            cache_write_usd_micros_per_mtok: None,
        }),
    },
    ModelInfo {
        provider: "azure_openai",
        id: squeezy_core::DEFAULT_AZURE_OPENAI_MODEL,
        profile: ModelProfile::Cheap,
        capabilities: ModelCapabilities {
            response_state: true,
            reasoning_tokens: true,
            reasoning_effort: true,
            text_verbosity: true,
            ..ModelCapabilities::TEXT_TOOLS
        },
        pricing: Some(TokenPricing {
            input_usd_micros_per_mtok: 50_000,
            output_usd_micros_per_mtok: 400_000,
            cache_read_usd_micros_per_mtok: Some(5_000),
            cache_write_usd_micros_per_mtok: None,
        }),
    },
    ModelInfo {
        provider: "bedrock",
        id: squeezy_core::DEFAULT_BEDROCK_MODEL,
        profile: ModelProfile::Cheap,
        capabilities: ModelCapabilities::TEXT_TOOLS,
        pricing: Some(TokenPricing {
            input_usd_micros_per_mtok: 800_000,
            output_usd_micros_per_mtok: 4_000_000,
            cache_read_usd_micros_per_mtok: Some(80_000),
            cache_write_usd_micros_per_mtok: Some(1_000_000),
        }),
    },
    ModelInfo {
        provider: "ollama",
        id: squeezy_core::DEFAULT_OLLAMA_MODEL,
        profile: ModelProfile::Cheap,
        capabilities: ModelCapabilities::TEXT_TOOLS,
        pricing: Some(TokenPricing {
            input_usd_micros_per_mtok: 0,
            output_usd_micros_per_mtok: 0,
            cache_read_usd_micros_per_mtok: Some(0),
            cache_write_usd_micros_per_mtok: Some(0),
        }),
    },
];

pub const PROVIDERS: &[&str] = &[
    "openai",
    "anthropic",
    "google",
    "azure_openai",
    "bedrock",
    "ollama",
];

pub fn models_for_provider(provider: &str) -> impl Iterator<Item = &'static ModelInfo> {
    MODEL_REGISTRY
        .iter()
        .filter(move |model| model.provider == provider)
}

pub fn capabilities_for(provider: &str, model: &str) -> Option<ModelCapabilities> {
    MODEL_REGISTRY
        .iter()
        .find(|entry| entry.provider == provider && entry.id == model)
        .map(|entry| entry.capabilities)
}

pub fn provider_name(config: &ProviderConfig) -> &'static str {
    match config {
        ProviderConfig::OpenAi(_) => "openai",
        ProviderConfig::Anthropic(_) => "anthropic",
        ProviderConfig::Google(_) => "google",
        ProviderConfig::AzureOpenAi(_) => "azure_openai",
        ProviderConfig::Bedrock(_) => "bedrock",
        ProviderConfig::Ollama(_) => "ollama",
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
    }
}

pub fn estimate_cost(provider: &str, model: &str, cost: &CostSnapshot) -> Option<u64> {
    let pricing = MODEL_REGISTRY
        .iter()
        .find(|entry| entry.provider == provider && entry.id == model)
        .and_then(|entry| entry.pricing)?;
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
