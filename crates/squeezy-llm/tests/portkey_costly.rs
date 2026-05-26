mod common;

use std::collections::BTreeMap;
use std::env;

use squeezy_core::{
    OpenAiCompatibleConfig, OpenAiCompatiblePreset, ProviderTransportConfig, Result, SqueezyError,
};
use squeezy_llm::OpenAiCompatibleProvider;

const PRESET: OpenAiCompatiblePreset = OpenAiCompatiblePreset::PortKey;
const API_KEY_ENV: &str = "PORTKEY_API_KEY";
const MODEL_ENV: &str = "SQUEEZY_COSTLY_PORTKEY_MODEL";
const VIRTUAL_KEY_ENV: &str = "PORTKEY_VIRTUAL_KEY";

#[tokio::test]
#[ignore = "costly: requires --features costly-tests, SQUEEZY_RUN_COSTLY_TESTS=1, PORTKEY_API_KEY, PORTKEY_VIRTUAL_KEY, and SQUEEZY_COSTLY_PORTKEY_MODEL"]
async fn portkey_chat_completions_streaming_costly() -> Result<()> {
    common::require_cargo_feature(common::COSTLY_FEATURE, cfg!(feature = "costly-tests"))?;
    common::require_env_flag(common::COSTLY_FLAG)?;
    common::require_env_key(API_KEY_ENV)?;
    common::require_env_key(VIRTUAL_KEY_ENV)?;

    // PortKey requires a virtual key header that points at the user's routing
    // config; the model id is whatever that virtual key resolves to.
    let model = env::var(MODEL_ENV)
        .or_else(|_| env::var("SQUEEZY_COSTLY_MODEL"))
        .map_err(|_| {
            SqueezyError::ProviderNotConfigured(format!(
                "PortKey costly test requires {MODEL_ENV} so the request can pick a backing model"
            ))
        })?;
    let mut extra_headers = BTreeMap::new();
    extra_headers.insert(
        "x-portkey-virtual-key".to_string(),
        env::var(VIRTUAL_KEY_ENV).unwrap_or_default(),
    );

    let provider = OpenAiCompatibleProvider::from_config(&OpenAiCompatibleConfig {
        preset: PRESET,
        api_key_env: API_KEY_ENV.to_string(),
        base_url: env::var("PORTKEY_BASE_URL")
            .unwrap_or_else(|_| PRESET.default_base_url().to_string()),
        extra_headers,
        transport: ProviderTransportConfig::default(),
    })?;
    let output = common::run_echo_smoke(provider, &model, PRESET.display_name()).await?;
    assert!(
        output.to_ascii_lowercase().contains("squeezy-ok"),
        "expected response to contain `squeezy-ok`, got: {output:?}"
    );
    Ok(())
}
