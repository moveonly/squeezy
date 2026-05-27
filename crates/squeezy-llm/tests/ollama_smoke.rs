//! Free, opt-in smoke test against a local Ollama daemon. Unlike the costly
//! tests this one does not call a paid API — it just talks to
//! `http://localhost:11434/api` (or whatever `OLLAMA_BASE_URL` resolves to).
//!
//! The test skips silently when:
//!   * `SQUEEZY_OLLAMA_SMOKE=1` is not set in the environment, **and**
//!   * the local daemon isn't reachable.
//!
//! With `SQUEEZY_OLLAMA_SMOKE=1` it fails if Ollama is missing — useful for
//! pre-release sanity from a host that's known to have Ollama running.

use std::env;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use squeezy_core::{
    DEFAULT_OLLAMA_BASE_URL, DEFAULT_OLLAMA_MODEL, OllamaConfig, ProviderTransportConfig, Result,
    SqueezyError,
};
use squeezy_llm::{LlmEvent, LlmInputItem, LlmProvider, LlmRequest, OllamaProvider};
use tokio_util::sync::CancellationToken;

const OPT_IN_ENV: &str = "SQUEEZY_OLLAMA_SMOKE";
const MODEL_ENV: &str = "SQUEEZY_OLLAMA_SMOKE_MODEL";

#[tokio::test]
async fn ollama_local_streaming_smoke() -> Result<()> {
    let required = env::var(OPT_IN_ENV).as_deref() == Ok("1");
    let base_url = env::var("OLLAMA_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string());

    if !is_reachable(&base_url).await {
        if required {
            return Err(SqueezyError::ProviderNotConfigured(format!(
                "{OPT_IN_ENV}=1 requested but Ollama at {base_url} is not reachable"
            )));
        }
        eprintln!("ollama smoke: {base_url} unreachable, skipping (set {OPT_IN_ENV}=1 to require)");
        return Ok(());
    }

    let provider = OllamaProvider::from_config(&OllamaConfig {
        base_url: base_url.clone(),
        transport: ProviderTransportConfig::default(),
    });
    let model = env::var(MODEL_ENV).unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string());
    let request = LlmRequest {
        model: Arc::from(model.as_str()),
        instructions: Arc::from("Reply with exactly: squeezy-ok"),
        input: Arc::from(vec![LlmInputItem::UserText(
            "Reply with exactly: squeezy-ok".to_string(),
        )]),
        max_output_tokens: Some(64),
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

    let mut stream = provider.stream_response(request, CancellationToken::new());
    let mut output = String::new();
    while let Some(event) = stream.next().await {
        match event? {
            LlmEvent::Started => {}
            LlmEvent::TextDelta(delta) => output.push_str(&delta),
            LlmEvent::ToolCall(tool_call) => {
                return Err(SqueezyError::ProviderStream(format!(
                    "ollama smoke returned unexpected tool call: {}",
                    tool_call.name
                )));
            }
            LlmEvent::Completed { .. } => break,
            LlmEvent::Cancelled => {
                return Err(SqueezyError::ProviderStream(
                    "ollama smoke was cancelled".to_string(),
                ));
            }
            LlmEvent::ReasoningDelta { .. } | LlmEvent::ReasoningDone(_) => {}
        }
    }

    if required {
        assert!(
            !output.trim().is_empty(),
            "ollama smoke expected a non-empty response, got: {output:?}"
        );
    } else {
        // Local models often miss the exact-string instruction; for the
        // opportunistic path we accept any non-error stream so daily runs on
        // hosts with whatever models the user has cached don't flake.
        eprintln!(
            "ollama smoke ok ({base_url}, model={model}): {} chars",
            output.len()
        );
    }
    Ok(())
}

async fn is_reachable(base_url: &str) -> bool {
    let url = format!("{}/tags", base_url.trim_end_matches('/'));
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(750))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };
    client.get(url).send().await.is_ok()
}
