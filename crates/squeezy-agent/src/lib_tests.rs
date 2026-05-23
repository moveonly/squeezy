use std::{pin::Pin, sync::Arc};

use futures_core::Stream;
use futures_util::stream;
use squeezy_core::{AppConfig, Result};
use squeezy_llm::{LlmEvent, LlmProvider, LlmRequest, LlmStream};

use super::*;

struct MockProvider {
    behavior: MockBehavior,
}

enum MockBehavior {
    Text,
    Error(String),
}

impl LlmProvider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn stream_response(&self, _request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        let events = match &self.behavior {
            MockBehavior::Text => vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::TextDelta("hel".to_string())),
                Ok(LlmEvent::TextDelta("lo".to_string())),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_1".to_string()),
                    cost: CostSnapshot::default(),
                }),
            ],
            MockBehavior::Error(message) => {
                vec![Err(SqueezyError::ProviderRequest(message.clone()))]
            }
        };
        let stream: Pin<Box<dyn Stream<Item = Result<LlmEvent>> + Send>> =
            Box::pin(stream::iter(events));
        stream
    }
}

#[tokio::test]
async fn turn_stream_accumulates_assistant_text() {
    let provider = Arc::new(MockProvider {
        behavior: MockBehavior::Text,
    });
    let agent = Agent::new(AppConfig::default(), provider);

    let mut rx = agent.start_turn("hi".to_string(), CancellationToken::new());
    let mut completed = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Completed {
            message,
            response_id,
            ..
        } = event
        {
            completed = Some((message.content, response_id));
        }
    }

    assert_eq!(
        completed,
        Some(("hello".to_string(), Some("resp_1".to_string())))
    );
}

#[tokio::test]
async fn turn_stream_reports_provider_error() {
    let provider = Arc::new(MockProvider {
        behavior: MockBehavior::Error("boom".to_string()),
    });
    let agent = Agent::new(AppConfig::default(), provider);

    let mut rx = agent.start_turn("hi".to_string(), CancellationToken::new());
    let mut saw_error = false;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Failed { error, .. } = event {
            saw_error = error.to_string().contains("boom");
        }
    }

    assert!(saw_error);
}
