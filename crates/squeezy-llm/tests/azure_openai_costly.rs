mod common;

use std::env;
use std::sync::Arc;

use squeezy_core::{
    AzureOpenAiConfig, DEFAULT_AZURE_OPENAI_API_VERSION, ProviderTransportConfig, Result,
    SqueezyError,
};
use squeezy_llm::{LlmInputItem, LlmProvider, LlmRequest, OpenAiProvider};
use tokio_util::sync::CancellationToken;

const API_KEY_ENV: &str = "AZURE_OPENAI_API_KEY";
const BASE_URL_ENV: &str = "AZURE_OPENAI_BASE_URL";
const API_VERSION_ENV: &str = "AZURE_OPENAI_API_VERSION";
const MODEL_ENV: &str = "SQUEEZY_COSTLY_AZURE_MODEL";

#[tokio::test]
#[ignore = "costly: requires --features costly-tests, SQUEEZY_RUN_COSTLY_TESTS=1, AZURE_OPENAI_API_KEY, AZURE_OPENAI_BASE_URL, and SQUEEZY_COSTLY_AZURE_MODEL"]
async fn azure_openai_responses_streaming_costly() -> Result<()> {
    common::require_cargo_feature(common::COSTLY_FEATURE, cfg!(feature = "costly-tests"))?;
    common::require_env_flag(common::COSTLY_FLAG)?;
    common::require_env_key(API_KEY_ENV)?;
    let base_url = env::var(BASE_URL_ENV).map_err(|_| {
        SqueezyError::ProviderNotConfigured(format!(
            "Azure OpenAI costly test requires {BASE_URL_ENV} (e.g. https://RESOURCE.openai.azure.com/openai/v1)"
        ))
    })?;
    let model = env::var(MODEL_ENV)
        .or_else(|_| env::var("SQUEEZY_COSTLY_MODEL"))
        .map_err(|_| {
            SqueezyError::ProviderNotConfigured(format!(
                "Azure OpenAI costly test requires {MODEL_ENV} to name the deployment id"
            ))
        })?;
    let api_version =
        env::var(API_VERSION_ENV).unwrap_or_else(|_| DEFAULT_AZURE_OPENAI_API_VERSION.to_string());

    let provider = OpenAiProvider::from_azure_config(&AzureOpenAiConfig {
        api_key_env: API_KEY_ENV.to_string(),
        api_key: None,
        base_url,
        api_version,
        transport: ProviderTransportConfig::default(),
    })?;
    let request = LlmRequest {
        model: Arc::from(model.as_str()),
        instructions: Arc::from("Reply with exactly: squeezy-ok"),
        input: Arc::from(vec![LlmInputItem::UserText(
            "Reply with exactly: squeezy-ok".to_string(),
        )]),
        max_output_tokens: common::costly_max_output_tokens()?,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: squeezy_llm::CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
    };
    let stream = provider.stream_response(request, CancellationToken::new());
    let output = common::collect_text(stream, "Azure OpenAI").await?;
    assert!(
        output.to_ascii_lowercase().contains("squeezy-ok"),
        "expected response to contain `squeezy-ok`, got: {output:?}"
    );
    Ok(())
}
