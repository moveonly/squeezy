use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use super::*;

#[tokio::test]
async fn unavailable_provider_reports_configuration_error() {
    let provider = UnavailableProvider::new("openai", "missing OPENAI_API_KEY");
    let request = LlmRequest {
        model: "test-model".to_string(),
        instructions: "test".to_string(),
        input: "hello".to_string(),
        max_output_tokens: Some(16),
        previous_response_id: None,
    };

    let mut stream = provider.stream_response(request, CancellationToken::new());
    let err = stream.next().await.expect("one event").expect_err("error");

    assert!(err.to_string().contains("missing OPENAI_API_KEY"));
    assert!(stream.next().await.is_none());
}
