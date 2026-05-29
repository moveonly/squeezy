//! Shared helpers for opt-in "costly" provider integration tests. Each file
//! under `crates/squeezy-llm/tests/*_costly.rs` declares `mod common;` and
//! reuses these guards so they're gated identically. Each test binary uses a
//! subset of the helpers; `#[allow(dead_code)]` silences "function unused"
//! warnings the per-binary unused-code analysis flags.

#![allow(dead_code)]

use std::env;
use std::sync::Arc;

use futures_util::StreamExt;
use squeezy_core::{DEFAULT_MAX_OUTPUT_TOKENS, Result, SqueezyError};
use squeezy_llm::{LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream};
use tokio_util::sync::CancellationToken;

pub const COSTLY_FLAG: &str = "SQUEEZY_RUN_COSTLY_TESTS";
pub const COSTLY_MAX_OUTPUT_TOKENS_ENV: &str = "SQUEEZY_COSTLY_MAX_OUTPUT_TOKENS";
pub const COSTLY_FEATURE: &str = "costly-tests";

pub fn require_cargo_feature(name: &str, enabled: bool) -> Result<()> {
    if enabled {
        return Ok(());
    }
    Err(SqueezyError::ProviderNotConfigured(format!(
        "costly integration test requires cargo --features {name}"
    )))
}

pub fn require_env_flag(name: &str) -> Result<()> {
    if env::var(name).as_deref() == Ok("1") {
        return Ok(());
    }
    Err(SqueezyError::ProviderNotConfigured(format!(
        "costly integration test requires {name}=1"
    )))
}

pub fn require_env_key(name: &str) -> Result<()> {
    if env::var(name).is_ok_and(|value| !value.trim().is_empty()) {
        return Ok(());
    }
    Err(SqueezyError::ProviderNotConfigured(format!(
        "costly integration test requires {name}"
    )))
}

pub fn costly_max_output_tokens() -> Result<Option<u32>> {
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

pub fn echo_request(model: &str, prompt: &str) -> LlmRequest {
    LlmRequest {
        model: Arc::from(model),
        instructions: Arc::from(prompt),
        input: Arc::from(vec![LlmInputItem::UserText(prompt.to_string())]),
        max_output_tokens: costly_max_output_tokens().unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
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
    }
}

pub async fn collect_text(mut stream: LlmStream, label: &str) -> Result<String> {
    let mut output = String::new();
    while let Some(event) = stream.next().await {
        match event? {
            LlmEvent::Started => {}
            LlmEvent::TextDelta(delta) => output.push_str(&delta),
            LlmEvent::ToolCall(tool_call) => {
                return Err(SqueezyError::ProviderStream(format!(
                    "{label} costly smoke test returned unexpected tool call: {}",
                    tool_call.name
                )));
            }
            LlmEvent::Completed { .. } => break,
            LlmEvent::Cancelled => {
                return Err(SqueezyError::ProviderStream(format!(
                    "{label} costly smoke test was cancelled"
                )));
            }
            LlmEvent::ReasoningDelta { .. }
            | LlmEvent::ReasoningDone(_)
            | LlmEvent::ContextOverflow { .. }
            | LlmEvent::ServerModel(_) => {}
        }
    }
    Ok(output)
}

pub async fn run_echo_smoke(
    provider: impl LlmProvider,
    model: &str,
    label: &str,
) -> Result<String> {
    let request = echo_request(model, "Reply with exactly: squeezy-ok");
    let stream = provider.stream_response(request, CancellationToken::new());
    collect_text(stream, label).await
}
