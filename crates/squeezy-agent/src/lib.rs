use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use futures_util::StreamExt;
use squeezy_core::{AppConfig, CostSnapshot, SqueezyError, TranscriptItem, TurnId};
use squeezy_llm::{LlmEvent, LlmProvider, LlmRequest};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Agent {
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    next_turn_id: Arc<AtomicU64>,
}

impl Agent {
    pub fn new(config: AppConfig, provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            config,
            provider,
            next_turn_id: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn provider_name(&self) -> &'static str {
        self.provider.name()
    }

    pub fn start_turn(
        &self,
        input: String,
        cancel: CancellationToken,
    ) -> mpsc::Receiver<AgentEvent> {
        let (tx, rx) = mpsc::channel(64);
        let provider = self.provider.clone();
        let request = LlmRequest {
            model: self.config.model.clone(),
            instructions: self.config.instructions.clone(),
            input: input.clone(),
            max_output_tokens: self.config.max_output_tokens,
            previous_response_id: None,
        };
        let turn_id = TurnId::new(self.next_turn_id.fetch_add(1, Ordering::Relaxed));

        tokio::spawn(async move {
            if tx
                .send(AgentEvent::UserMessage {
                    turn_id,
                    message: TranscriptItem::user(input),
                })
                .await
                .is_err()
            {
                return;
            }

            let mut assistant_text = String::new();
            let mut stream = provider.stream_response(request, cancel.clone());
            while let Some(event) = stream.next().await {
                match event {
                    Ok(LlmEvent::Started) => {
                        if tx.send(AgentEvent::Started { turn_id }).await.is_err() {
                            return;
                        }
                    }
                    Ok(LlmEvent::TextDelta(delta)) => {
                        assistant_text.push_str(&delta);
                        if tx
                            .send(AgentEvent::AssistantDelta { turn_id, delta })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(LlmEvent::Completed { response_id, cost }) => {
                        let _ = tx
                            .send(AgentEvent::Completed {
                                turn_id,
                                message: TranscriptItem::assistant(assistant_text),
                                response_id,
                                cost,
                            })
                            .await;
                        return;
                    }
                    Ok(LlmEvent::Cancelled) => {
                        let _ = tx.send(AgentEvent::Cancelled { turn_id }).await;
                        return;
                    }
                    Err(error) => {
                        let _ = tx.send(AgentEvent::Failed { turn_id, error }).await;
                        return;
                    }
                }
            }

            let _ = tx
                .send(AgentEvent::Completed {
                    turn_id,
                    message: TranscriptItem::assistant(assistant_text),
                    response_id: None,
                    cost: CostSnapshot::default(),
                })
                .await;
        });

        rx
    }
}

#[derive(Debug)]
pub enum AgentEvent {
    UserMessage {
        turn_id: TurnId,
        message: TranscriptItem,
    },
    Started {
        turn_id: TurnId,
    },
    AssistantDelta {
        turn_id: TurnId,
        delta: String,
    },
    Completed {
        turn_id: TurnId,
        message: TranscriptItem,
        response_id: Option<String>,
        cost: CostSnapshot,
    },
    Cancelled {
        turn_id: TurnId,
    },
    Failed {
        turn_id: TurnId,
        error: SqueezyError,
    },
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
