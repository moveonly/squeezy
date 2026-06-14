use crate::{
    DEFAULT_ANTHROPIC_BASE_URL, DEFAULT_BEDROCK_REGION, DEFAULT_GOOGLE_BASE_URL,
    DEFAULT_OLLAMA_BASE_URL, DEFAULT_OPENAI_BASE_URL, OpenAiCompatiblePreset, Result, SqueezyError,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BaseProviderMeta {
    pub name: &'static str,
    pub display_name: &'static str,
    pub base_url: &'static str,
    pub api_key_env: &'static str,
    pub aliases: &'static [&'static str],
}

pub const BASE_PROVIDER_METADATA: &[BaseProviderMeta] = &[
    BaseProviderMeta {
        name: "openai",
        display_name: "OpenAI",
        base_url: DEFAULT_OPENAI_BASE_URL,
        api_key_env: "OPENAI_API_KEY",
        aliases: &["openai", "open_ai"],
    },
    BaseProviderMeta {
        name: "anthropic",
        display_name: "Anthropic",
        base_url: DEFAULT_ANTHROPIC_BASE_URL,
        api_key_env: "ANTHROPIC_API_KEY",
        aliases: &["anthropic", "claude"],
    },
    BaseProviderMeta {
        name: "google",
        display_name: "Google AI Studio",
        base_url: DEFAULT_GOOGLE_BASE_URL,
        api_key_env: "GOOGLE_API_KEY",
        aliases: &["google", "gemini", "google_ai", "google_ai_studio"],
    },
    BaseProviderMeta {
        name: "azure_openai",
        display_name: "Azure OpenAI",
        base_url: "",
        api_key_env: "AZURE_OPENAI_API_KEY",
        aliases: &["azure_openai", "azure", "azure_ai"],
    },
    BaseProviderMeta {
        name: "bedrock",
        display_name: "AWS Bedrock",
        base_url: DEFAULT_BEDROCK_REGION,
        api_key_env: "",
        aliases: &["bedrock", "aws_bedrock", "aws"],
    },
    BaseProviderMeta {
        name: "ollama",
        display_name: "Ollama",
        base_url: DEFAULT_OLLAMA_BASE_URL,
        api_key_env: "",
        aliases: &["ollama"],
    },
    BaseProviderMeta {
        name: "github_copilot",
        display_name: "GitHub Copilot",
        base_url: "(token-derived)",
        api_key_env: "squeezy auth github-copilot login",
        aliases: &["github_copilot", "github-copilot", "copilot"],
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderAuthMeta {
    pub section: &'static str,
    pub cli: &'static str,
    pub env: &'static str,
    pub fallback_env: Option<&'static str>,
}

const STATIC_AUTH_METADATA: &[ProviderAuthMeta] = &[
    ProviderAuthMeta {
        section: "openai",
        cli: "openai",
        env: "SQUEEZY_OPENAI_KEY",
        fallback_env: Some("OPENAI_API_KEY"),
    },
    ProviderAuthMeta {
        section: "anthropic",
        cli: "anthropic",
        env: "SQUEEZY_ANTHROPIC_KEY",
        fallback_env: Some("ANTHROPIC_API_KEY"),
    },
    ProviderAuthMeta {
        section: "google",
        cli: "google",
        env: "SQUEEZY_GOOGLE_KEY",
        fallback_env: Some("GOOGLE_API_KEY"),
    },
    ProviderAuthMeta {
        section: "azure_openai",
        cli: "azure",
        env: "SQUEEZY_AZURE_OPENAI_KEY",
        fallback_env: Some("AZURE_OPENAI_API_KEY"),
    },
];

const PRESET_AUTH_METADATA: &[ProviderAuthMeta] = &[
    ProviderAuthMeta {
        section: "openrouter",
        cli: "openrouter",
        env: "SQUEEZY_OPENROUTER_KEY",
        fallback_env: Some("OPENROUTER_API_KEY"),
    },
    ProviderAuthMeta {
        section: "vercel",
        cli: "vercel",
        env: "SQUEEZY_VERCEL_KEY",
        fallback_env: Some("AI_GATEWAY_API_KEY"),
    },
    ProviderAuthMeta {
        section: "portkey",
        cli: "portkey",
        env: "SQUEEZY_PORTKEY_KEY",
        fallback_env: Some("PORTKEY_API_KEY"),
    },
    ProviderAuthMeta {
        section: "groq",
        cli: "groq",
        env: "SQUEEZY_GROQ_KEY",
        fallback_env: Some("GROQ_API_KEY"),
    },
    ProviderAuthMeta {
        section: "xai",
        cli: "xai",
        env: "SQUEEZY_XAI_KEY",
        fallback_env: Some("XAI_API_KEY"),
    },
    ProviderAuthMeta {
        section: "deepseek",
        cli: "deepseek",
        env: "SQUEEZY_DEEPSEEK_KEY",
        fallback_env: Some("DEEPSEEK_API_KEY"),
    },
    ProviderAuthMeta {
        section: "vertex",
        cli: "vertex",
        env: "SQUEEZY_VERTEX_KEY",
        fallback_env: Some("VERTEX_ACCESS_TOKEN"),
    },
    ProviderAuthMeta {
        section: "mistral",
        cli: "mistral",
        env: "SQUEEZY_MISTRAL_KEY",
        fallback_env: Some("MISTRAL_API_KEY"),
    },
    ProviderAuthMeta {
        section: "together",
        cli: "together",
        env: "SQUEEZY_TOGETHER_KEY",
        fallback_env: Some("TOGETHER_API_KEY"),
    },
    ProviderAuthMeta {
        section: "fireworks",
        cli: "fireworks",
        env: "SQUEEZY_FIREWORKS_KEY",
        fallback_env: Some("FIREWORKS_API_KEY"),
    },
    ProviderAuthMeta {
        section: "cerebras",
        cli: "cerebras",
        env: "SQUEEZY_CEREBRAS_KEY",
        fallback_env: Some("CEREBRAS_API_KEY"),
    },
    ProviderAuthMeta {
        section: "deepinfra",
        cli: "deepinfra",
        env: "DEEPINFRA_API_KEY",
        fallback_env: Some("DEEPINFRA_TOKEN"),
    },
    ProviderAuthMeta {
        section: "baseten",
        cli: "baseten",
        env: "BASETEN_API_KEY",
        fallback_env: None,
    },
    ProviderAuthMeta {
        section: "lmstudio",
        cli: "lmstudio",
        env: "SQUEEZY_LMSTUDIO_KEY",
        fallback_env: Some("LMSTUDIO_API_KEY"),
    },
    ProviderAuthMeta {
        section: "vllm",
        cli: "vllm",
        env: "SQUEEZY_VLLM_KEY",
        fallback_env: Some("VLLM_API_KEY"),
    },
    ProviderAuthMeta {
        section: "llamacpp",
        cli: "llamacpp",
        env: "SQUEEZY_LLAMACPP_KEY",
        fallback_env: Some("LLAMACPP_API_KEY"),
    },
    ProviderAuthMeta {
        section: "cloudflare_workers_ai",
        cli: "cloudflare_workers_ai",
        env: "SQUEEZY_CLOUDFLARE_WORKERS_AI_KEY",
        fallback_env: Some("CLOUDFLARE_API_KEY"),
    },
    ProviderAuthMeta {
        section: "cloudflare_ai_gateway",
        cli: "cloudflare_ai_gateway",
        env: "SQUEEZY_CLOUDFLARE_AI_GATEWAY_KEY",
        fallback_env: Some("CLOUDFLARE_API_KEY"),
    },
    ProviderAuthMeta {
        section: "openai_compatible",
        cli: "openai_compatible",
        env: "SQUEEZY_OPENAI_COMPATIBLE_KEY",
        fallback_env: None,
    },
];

pub fn provider_auth_metadata() -> Vec<ProviderAuthMeta> {
    let mut entries = STATIC_AUTH_METADATA.to_vec();
    entries.extend(PRESET_AUTH_METADATA);
    entries
}

pub fn provider_auth_for_section(section: &str) -> Option<ProviderAuthMeta> {
    provider_auth_metadata()
        .into_iter()
        .find(|meta| meta.section == section)
}

pub fn canonical_provider_name(value: &str) -> Option<&'static str> {
    let trimmed = value.trim();
    let lower = trimmed.to_ascii_lowercase();
    for entry in BASE_PROVIDER_METADATA {
        if entry.aliases.iter().any(|alias| *alias == lower) {
            return Some(entry.name);
        }
    }
    OpenAiCompatiblePreset::parse(trimmed).map(|preset| preset.as_str())
}

/// Map a CLI provider id to the `[providers.<section>]` TOML section that can
/// hold inline authentication metadata.
pub fn provider_section_for_cli(provider: &str) -> Result<&'static str> {
    match provider {
        "openai" => Ok("openai"),
        "anthropic" | "claude" => Ok("anthropic"),
        "google" | "gemini" => Ok("google"),
        "azure" | "azure-openai" | "azure_openai" => Ok("azure_openai"),
        "bedrock" => Err(SqueezyError::Config(
            "bedrock uses the AWS default credential chain; configure credentials with aws configure"
                .to_string(),
        )),
        "ollama" | "local" => Err(SqueezyError::Config(
            "ollama runs locally and does not require an API key".to_string(),
        )),
        other => Ok(OpenAiCompatiblePreset::parse(other)
            .map(|preset| preset.as_str())
            .unwrap_or("")),
    }
}

pub fn env_value_set(env_lookup: &dyn Fn(&str) -> Option<String>, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    env_lookup(name)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

/// Best-effort environment check for the AWS credential chain.
pub fn bedrock_configured(env_lookup: &dyn Fn(&str) -> Option<String>) -> bool {
    const AWS_CRED_VARS: &[&str] = &[
        "AWS_ACCESS_KEY_ID",
        "AWS_PROFILE",
        "AWS_DEFAULT_PROFILE",
        "AWS_ROLE_ARN",
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        "AWS_BEARER_TOKEN_BEDROCK",
        "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    ];
    AWS_CRED_VARS
        .iter()
        .any(|var| env_value_set(env_lookup, var))
}
