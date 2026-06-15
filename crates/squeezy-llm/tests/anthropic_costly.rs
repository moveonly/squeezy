mod common;

use std::env;

use squeezy_core::{
    AnthropicConfig, DEFAULT_ANTHROPIC_BASE_URL, DEFAULT_ANTHROPIC_MODEL, ProviderTransportConfig,
    Result,
};
use squeezy_llm::{AnthropicProvider, LlmInputItem, LlmProvider, LlmRequest};
use tokio_util::sync::CancellationToken;

const ANTHROPIC_KEY_ENV: &str = "ANTHROPIC_API_KEY";

#[tokio::test]
#[ignore = "costly: requires --features costly-tests, SQUEEZY_RUN_COSTLY_TESTS=1, and ANTHROPIC_API_KEY"]
async fn anthropic_messages_streaming_costly() -> Result<()> {
    common::require_cargo_feature(common::COSTLY_FEATURE, cfg!(feature = "costly-tests"))?;
    common::require_env_flag(common::COSTLY_FLAG)?;
    common::require_env_key(ANTHROPIC_KEY_ENV)?;

    let provider = AnthropicProvider::from_config(&AnthropicConfig {
        api_key_env: ANTHROPIC_KEY_ENV.to_string(),
        api_key: None,
        base_url: env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_ANTHROPIC_BASE_URL.to_string()),
        transport: ProviderTransportConfig::default(),
    })?;
    let request = LlmRequest {
        model: std::sync::Arc::from(
            env::var("SQUEEZY_COSTLY_ANTHROPIC_MODEL")
                .or_else(|_| env::var("SQUEEZY_COSTLY_MODEL"))
                .unwrap_or_else(|_| DEFAULT_ANTHROPIC_MODEL.to_string())
                .as_str(),
        ),
        instructions: std::sync::Arc::from("Reply with exactly: squeezy-ok"),
        input: std::sync::Arc::from(vec![LlmInputItem::UserText(
            "Reply with exactly: squeezy-ok".to_string(),
        )]),
        max_output_tokens: common::costly_max_output_tokens()?,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: squeezy_llm::CacheSpec::default(),
        tools: std::sync::Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let stream = provider.stream_response(request, CancellationToken::new());
    let output = common::collect_text(stream, "Anthropic").await?;
    common::assert_echo_response(&output);

    Ok(())
}
