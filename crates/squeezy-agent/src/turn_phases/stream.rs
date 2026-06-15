//! Provider stream-loop helpers for `TurnRuntime`.
//!
//! The stream phase owns cancellation-aware polling and timeout handling for
//! provider events before the run loop dispatches those events.

use super::super::*;

pub(crate) async fn next_llm_stream_event(
    stream: &mut LlmStream,
    cancel: &CancellationToken,
    idle_timeout: Duration,
) -> squeezy_core::Result<Option<LlmEvent>> {
    let next = tokio::select! {
        _ = cancel.cancelled() => return Ok(Some(LlmEvent::Cancelled)),
        next = tokio::time::timeout(idle_timeout, stream.next()) => next,
    };
    match next {
        Ok(Some(event)) => event.map(Some),
        Ok(None) => Ok(None),
        Err(_) => Err(SqueezyError::ProviderStream(format!(
            "idle timeout waiting for model stream after {}ms",
            idle_timeout.as_millis()
        ))),
    }
}
