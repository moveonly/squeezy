use std::env;

use futures_util::StreamExt;
use squeezy_core::{
    DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_OPENAI_BASE_URL, DEFAULT_OPENAI_MODEL, OpenAiConfig, Result,
    SqueezyError,
};
use squeezy_llm::{LlmEvent, LlmInputItem, LlmProvider, LlmRequest, OpenAiProvider};
use tokio_util::sync::CancellationToken;

const COSTLY_FLAG: &str = "SQUEEZY_RUN_COSTLY_TESTS";
const OPENAI_KEY_ENV: &str = "OPENAI_API_KEY";
const COSTLY_MAX_OUTPUT_TOKENS_ENV: &str = "SQUEEZY_COSTLY_MAX_OUTPUT_TOKENS";
const COSTLY_FEATURE: &str = "costly-tests";

#[tokio::test]
#[ignore = "costly: requires --features costly-tests, SQUEEZY_RUN_COSTLY_TESTS=1, and OPENAI_API_KEY"]
async fn openai_responses_streaming_costly() -> Result<()> {
    require_cargo_feature(COSTLY_FEATURE, cfg!(feature = "costly-tests"))?;
    require_env_flag(COSTLY_FLAG)?;
    require_env_key(OPENAI_KEY_ENV)?;

    let provider = OpenAiProvider::from_config(&OpenAiConfig {
        api_key_env: OPENAI_KEY_ENV.to_string(),
        api_key_keychain: None,
        base_url: env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_OPENAI_BASE_URL.to_string()),
    })?;
    let request = LlmRequest {
        model: env::var("SQUEEZY_COSTLY_MODEL")
            .unwrap_or_else(|_| DEFAULT_OPENAI_MODEL.to_string()),
        instructions: "Reply with exactly: squeezy-ok".to_string(),
        input: vec![LlmInputItem::UserText(
            "Reply with exactly: squeezy-ok".to_string(),
        )],
        max_output_tokens: costly_max_output_tokens()?,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        tools: Vec::new(),
        store: false,
    };

    let mut stream = provider.stream_response(request, CancellationToken::new());
    let mut output = String::new();
    while let Some(event) = stream.next().await {
        match event? {
            LlmEvent::Started => {}
            LlmEvent::TextDelta(delta) => output.push_str(&delta),
            LlmEvent::ToolCall(_) => {
                return Err(SqueezyError::ProviderStream(
                    "costly OpenAI smoke test should not call tools".to_string(),
                ));
            }
            LlmEvent::Completed { .. } => break,
            LlmEvent::Cancelled => {
                return Err(SqueezyError::ProviderStream(
                    "costly OpenAI smoke test was cancelled".to_string(),
                ));
            }
        }
    }

    assert!(
        output.to_ascii_lowercase().contains("squeezy-ok"),
        "expected response to contain `squeezy-ok`, got: {output:?}"
    );

    Ok(())
}

fn require_cargo_feature(name: &str, enabled: bool) -> Result<()> {
    if enabled {
        return Ok(());
    }

    Err(SqueezyError::ProviderNotConfigured(format!(
        "costly integration test requires cargo --features {name}"
    )))
}

fn require_env_flag(name: &str) -> Result<()> {
    if env::var(name).as_deref() == Ok("1") {
        return Ok(());
    }

    Err(SqueezyError::ProviderNotConfigured(format!(
        "costly integration test requires {name}=1"
    )))
}

fn require_env_key(name: &str) -> Result<()> {
    if env::var(name).is_ok_and(|value| !value.trim().is_empty()) {
        return Ok(());
    }

    Err(SqueezyError::ProviderNotConfigured(format!(
        "costly integration test requires {name}"
    )))
}

fn costly_max_output_tokens() -> Result<Option<u32>> {
    let Ok(raw) = env::var(COSTLY_MAX_OUTPUT_TOKENS_ENV) else {
        return Ok(DEFAULT_MAX_OUTPUT_TOKENS);
    };

    let parsed = raw.parse::<u32>().map_err(|_| {
        SqueezyError::ProviderNotConfigured(format!(
            "costly integration test requires {COSTLY_MAX_OUTPUT_TOKENS_ENV} to be a positive integer"
        ))
    })?;

    if parsed == 0 {
        return Err(SqueezyError::ProviderNotConfigured(format!(
            "costly integration test requires {COSTLY_MAX_OUTPUT_TOKENS_ENV} to be greater than 0"
        )));
    }

    Ok(Some(parsed))
}
