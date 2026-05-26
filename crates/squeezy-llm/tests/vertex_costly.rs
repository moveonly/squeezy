mod common;

use std::collections::BTreeMap;
use std::env;

use squeezy_core::{
    DEFAULT_VERTEX_LOCATION, DEFAULT_VERTEX_MODEL, OpenAiCompatibleConfig, OpenAiCompatiblePreset,
    ProviderTransportConfig, Result, SqueezyError, vertex_base_url,
};
use squeezy_llm::OpenAiCompatibleProvider;

const PRESET: OpenAiCompatiblePreset = OpenAiCompatiblePreset::Vertex;
const API_KEY_ENV: &str = "VERTEX_ACCESS_TOKEN";
const MODEL_ENV: &str = "SQUEEZY_COSTLY_VERTEX_MODEL";
const PROJECT_ENV: &str = "VERTEX_PROJECT";

#[tokio::test]
#[ignore = "costly: requires --features costly-tests, SQUEEZY_RUN_COSTLY_TESTS=1, VERTEX_ACCESS_TOKEN (refresh via `gcloud auth print-access-token`), and VERTEX_PROJECT"]
async fn vertex_chat_completions_streaming_costly() -> Result<()> {
    common::require_cargo_feature(common::COSTLY_FEATURE, cfg!(feature = "costly-tests"))?;
    common::require_env_flag(common::COSTLY_FLAG)?;
    common::require_env_key(API_KEY_ENV)?;
    common::require_env_key(PROJECT_ENV)?;
    let project = env::var(PROJECT_ENV).unwrap_or_default();
    let location =
        env::var("VERTEX_LOCATION").unwrap_or_else(|_| DEFAULT_VERTEX_LOCATION.to_string());
    if project.trim().is_empty() {
        return Err(SqueezyError::ProviderNotConfigured(format!(
            "Vertex costly test requires {PROJECT_ENV} to point at a GCP project id"
        )));
    }
    let base_url = env::var("VERTEX_BASE_URL")
        .unwrap_or_else(|_| vertex_base_url(project.trim(), location.trim()));

    let provider = OpenAiCompatibleProvider::from_config(&OpenAiCompatibleConfig {
        preset: PRESET,
        api_key_env: API_KEY_ENV.to_string(),
        api_key: None,
        base_url,
        extra_headers: BTreeMap::new(),
        transport: ProviderTransportConfig::default(),
    })?;
    let model = env::var(MODEL_ENV)
        .or_else(|_| env::var("SQUEEZY_COSTLY_MODEL"))
        .unwrap_or_else(|_| DEFAULT_VERTEX_MODEL.to_string());
    let output = common::run_echo_smoke(provider, &model, PRESET.display_name()).await?;
    assert!(
        output.to_ascii_lowercase().contains("squeezy-ok"),
        "expected response to contain `squeezy-ok`, got: {output:?}"
    );
    Ok(())
}
