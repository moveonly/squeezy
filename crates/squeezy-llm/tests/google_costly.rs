mod common;

use std::env;
use std::sync::Arc;

use squeezy_core::{
    DEFAULT_GOOGLE_BASE_URL, DEFAULT_GOOGLE_MODEL, GoogleConfig, ProviderTransportConfig, Result,
};
use squeezy_llm::{GoogleProvider, LlmInputItem, LlmProvider, LlmRequest};
use tokio_util::sync::CancellationToken;

const API_KEY_ENV: &str = "GEMINI_API_KEY";

#[tokio::test]
#[ignore = "costly: requires --features costly-tests, SQUEEZY_RUN_COSTLY_TESTS=1, and GEMINI_API_KEY (or GOOGLE_API_KEY)"]
async fn google_gemini_streaming_costly() -> Result<()> {
    common::require_cargo_feature(common::COSTLY_FEATURE, cfg!(feature = "costly-tests"))?;
    common::require_env_flag(common::COSTLY_FLAG)?;
    let resolved_env = if env::var(API_KEY_ENV).is_ok_and(|value| !value.trim().is_empty()) {
        API_KEY_ENV
    } else if env::var("GOOGLE_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
        "GOOGLE_API_KEY"
    } else {
        common::require_env_key(API_KEY_ENV)?;
        unreachable!("require_env_key returns error on miss");
    };

    let provider = GoogleProvider::from_config(&GoogleConfig {
        api_key_env: resolved_env.to_string(),
        api_key_keychain: None,
        base_url: env::var("GOOGLE_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_GOOGLE_BASE_URL.to_string()),
        transport: ProviderTransportConfig::default(),
    })?;
    // Default model is `gemini-2.5-pro`. Gemini 2.5 Flash is the cheaper
    // alternative — let callers override either via vendor- or generic-named
    // env vars before falling back to the package default.
    let model = env::var("SQUEEZY_COSTLY_GOOGLE_MODEL")
        .or_else(|_| env::var("SQUEEZY_COSTLY_MODEL"))
        .unwrap_or_else(|_| DEFAULT_GOOGLE_MODEL.to_string());
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
    };
    let stream = provider.stream_response(request, CancellationToken::new());
    let output = common::collect_text(stream, "Google Gemini").await?;
    assert!(
        output.to_ascii_lowercase().contains("squeezy-ok"),
        "expected response to contain `squeezy-ok`, got: {output:?}"
    );
    Ok(())
}
