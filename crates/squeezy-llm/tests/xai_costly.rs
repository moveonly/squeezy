mod common;

use std::collections::BTreeMap;
use std::env;

use squeezy_core::{
    OpenAiCompatibleConfig, OpenAiCompatiblePreset, ProviderTransportConfig, Result,
};
use squeezy_llm::OpenAiCompatibleProvider;

const PRESET: OpenAiCompatiblePreset = OpenAiCompatiblePreset::XAi;
const API_KEY_ENV: &str = "XAI_API_KEY";
const MODEL_ENV: &str = "SQUEEZY_COSTLY_XAI_MODEL";
const DEFAULT_MODEL: &str = "grok-4-fast-non-reasoning";

#[tokio::test]
#[ignore = "costly: requires --features costly-tests, SQUEEZY_RUN_COSTLY_TESTS=1, and XAI_API_KEY"]
async fn xai_chat_completions_streaming_costly() -> Result<()> {
    common::require_cargo_feature(common::COSTLY_FEATURE, cfg!(feature = "costly-tests"))?;
    common::require_env_flag(common::COSTLY_FLAG)?;
    common::require_env_key(API_KEY_ENV)?;

    let provider = OpenAiCompatibleProvider::from_config(&OpenAiCompatibleConfig {
        preset: PRESET,
        api_key_env: API_KEY_ENV.to_string(),
        api_key: None,
        base_url: env::var("XAI_BASE_URL")
            .unwrap_or_else(|_| PRESET.default_base_url().to_string()),
        extra_headers: BTreeMap::new(),
        transport: ProviderTransportConfig::default(),
        account_id: None,
        gateway_id: None,
    })?;
    let model = env::var(MODEL_ENV)
        .or_else(|_| env::var("SQUEEZY_COSTLY_MODEL"))
        .unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let output = common::run_echo_smoke(provider, &model, PRESET.display_name()).await?;
    assert!(
        output.to_ascii_lowercase().contains("squeezy-ok"),
        "expected response to contain `squeezy-ok`, got: {output:?}"
    );
    Ok(())
}
