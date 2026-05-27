mod common;

use std::env;
use std::sync::Arc;

use squeezy_core::{
    BedrockConfig, DEFAULT_BEDROCK_MODEL, DEFAULT_BEDROCK_REGION, ProviderTransportConfig, Result,
};
use squeezy_llm::{BedrockProvider, LlmInputItem, LlmProvider, LlmRequest};
use tokio_util::sync::CancellationToken;

const MODEL_ENV: &str = "SQUEEZY_COSTLY_BEDROCK_MODEL";
const REGION_ENV: &str = "AWS_REGION";

#[tokio::test]
#[ignore = "costly: requires --features costly-tests, SQUEEZY_RUN_COSTLY_TESTS=1, working AWS credentials, and AWS_REGION"]
async fn bedrock_converse_streaming_costly() -> Result<()> {
    common::require_cargo_feature(common::COSTLY_FEATURE, cfg!(feature = "costly-tests"))?;
    common::require_env_flag(common::COSTLY_FLAG)?;
    // The AWS SDK pulls credentials from the standard chain (env, shared
    // config, IMDS) so we only need to assert a region is set; the SDK errors
    // out clearly when no credentials resolve.
    common::require_env_key(REGION_ENV)?;

    let region = env::var(REGION_ENV).unwrap_or_else(|_| DEFAULT_BEDROCK_REGION.to_string());
    let provider = BedrockProvider::from_config(&BedrockConfig {
        region,
        base_url: env::var("BEDROCK_BASE_URL").ok(),
        transport: ProviderTransportConfig::default(),
    })?;
    let model = env::var(MODEL_ENV)
        .or_else(|_| env::var("SQUEEZY_COSTLY_MODEL"))
        .unwrap_or_else(|_| DEFAULT_BEDROCK_MODEL.to_string());
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
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
    };
    let stream = provider.stream_response(request, CancellationToken::new());
    let output = common::collect_text(stream, "Amazon Bedrock").await?;
    assert!(
        output.to_ascii_lowercase().contains("squeezy-ok"),
        "expected response to contain `squeezy-ok`, got: {output:?}"
    );
    Ok(())
}
