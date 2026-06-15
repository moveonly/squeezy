use async_stream::try_stream;
use futures_util::StreamExt;
use squeezy_core::{Result, SqueezyError};
use tokio_util::sync::CancellationToken;

use super::classifier::is_retryable_stream_error;
use super::policy::{RetryPolicy, capped_backoff, sleep_or_cancel};
use crate::{LlmEvent, LlmStream};

/// Tracks the already-emitted prefix of a provider stream so a restart
/// attempt can suppress the events the caller has already observed - and,
/// crucially, so it can verify that the regenerated attempt actually
/// reproduces that prefix. Provider streams are sampled independently on
/// every reconnect (no seed / temperature is pinned), so attempt N+1 can
/// diverge from N; skipping by raw counts alone would splice two different
/// generations into one corrupted turn. We retain the emitted content
/// (text, reasoning, tool-call ids) rather than just counts so divergence
/// is detectable.
#[derive(Debug, Default, Clone)]
pub struct StreamSkipState {
    /// Concatenated `TextDelta` content emitted across attempts.
    emitted_text: String,
    /// Concatenated `ReasoningDelta` content emitted across attempts.
    emitted_reasoning: String,
    /// Number of completed `ReasoningDone` events emitted.
    emitted_reasoning_done: usize,
    /// `call_id`s of the completed `ToolCall` events emitted, in order.
    emitted_tool_call_ids: Vec<String>,
    /// Whether `Started` has been emitted to the downstream consumer.
    started: bool,
    /// Whether a `ServerModel` event has already reached the
    /// downstream consumer on this stream. The event is at-most-once
    /// per turn - a mid-stream reconnect re-runs the provider's
    /// first-frame parsing and would otherwise yield the same echo
    /// again on attempt N+1. Suppress the duplicate so consumers
    /// (TUI, transcript writer) see one notification per turn.
    emitted_server_model: bool,
}

impl StreamSkipState {
    /// Update tracked state for an event that just got yielded downstream.
    fn observe_yielded(&mut self, event: &LlmEvent) {
        match event {
            LlmEvent::Started => self.started = true,
            LlmEvent::TextDelta(text) => self.emitted_text.push_str(text),
            LlmEvent::ReasoningDelta { text, .. } => self.emitted_reasoning.push_str(text),
            LlmEvent::ReasoningDone(_) => self.emitted_reasoning_done += 1,
            LlmEvent::ToolCall(call) => self.emitted_tool_call_ids.push(call.call_id.clone()),
            LlmEvent::ServerModel(_) => self.emitted_server_model = true,
            LlmEvent::Completed { .. } | LlmEvent::Cancelled | LlmEvent::ContextOverflow { .. } => {
            }
            _ => {}
        }
    }

    /// True when nothing the consumer must keep has been committed yet - only
    /// `Started`, a `ServerModel` echo, and/or reasoning deltas have reached
    /// it. Reasoning is ephemeral live-display content, so a fresh restart
    /// that re-streams it is cosmetic; visible `TextDelta` or a `ToolCall`,
    /// by contrast, the consumer has already folded into the committed turn
    /// and cannot un-see.
    fn is_uncommitted(&self) -> bool {
        self.emitted_text.is_empty() && self.emitted_tool_call_ids.is_empty()
    }

    /// Discard the tracked content prefix so the next attempt streams from
    /// scratch, while keeping the one-shot `started`/`server_model` latches so
    /// the consumer doesn't see a duplicate turn-start or model echo. Used to
    /// recover an early reconnect divergence as a clean restart instead of a
    /// fatal turn error. Only sound when [`Self::is_uncommitted`] holds.
    fn reset_content_for_restart(&mut self) {
        self.emitted_text.clear();
        self.emitted_reasoning.clear();
        self.emitted_reasoning_done = 0;
        self.emitted_tool_call_ids.clear();
    }
}

/// True when `err` is a splice-divergence raised by the reconnect cursor (the
/// regenerated text/reasoning/tool-call did not reproduce the already-emitted
/// prefix), as opposed to an ordinary transport-level stream drop. These are
/// the only errors a clean restart can recover.
fn is_reconnect_divergence(err: &SqueezyError) -> bool {
    matches!(
        err,
        SqueezyError::ProviderStream(msg) if msg.contains("stream reconnect diverged")
    )
}

/// Per-attempt cursor that tracks how much of the already-emitted prefix the
/// freshly-restarted provider stream has reproduced and decides what should
/// be passed through to the caller. `expected_*` snapshot the prefix lengths
/// recorded *before* this attempt began, so [`SkipCursor::check_complete`]
/// can tell a truncated regeneration apart from the suffix this attempt
/// legitimately appended.
#[derive(Debug, Default)]
struct SkipCursor {
    seen_text_chars: usize,
    seen_reasoning_chars: usize,
    seen_reasoning_done: usize,
    seen_tool_calls: usize,
    expected_text_chars: usize,
    expected_reasoning_chars: usize,
    expected_tool_calls: usize,
}

impl SkipCursor {
    /// Snapshot the prefix the consumer has already observed so this attempt
    /// can be checked for reproducing it in full.
    fn for_attempt(skip: &StreamSkipState) -> Self {
        Self {
            expected_text_chars: skip.emitted_text.chars().count(),
            expected_reasoning_chars: skip.emitted_reasoning.chars().count(),
            expected_tool_calls: skip.emitted_tool_call_ids.len(),
            ..Self::default()
        }
    }

    /// Returns `Ok(Some(event))` to pass through, `Ok(None)` to suppress an
    /// event that re-covers ground a previous attempt already streamed, or
    /// `Err(_)` when the restarted attempt diverges from the recorded prefix
    /// (different wording or a different tool-call id). Splicing a divergent
    /// continuation onto the already-emitted prefix would corrupt the turn,
    /// so divergence surfaces as a stream error instead.
    fn filter(&mut self, event: LlmEvent, skip: &StreamSkipState) -> Result<Option<LlmEvent>> {
        match event {
            LlmEvent::Started => {
                if skip.started {
                    Ok(None)
                } else {
                    Ok(Some(LlmEvent::Started))
                }
            }
            LlmEvent::TextDelta(text) => {
                let forwarded =
                    skip_validated_prefix(text, &skip.emitted_text, &mut self.seen_text_chars)?;
                Ok(forwarded.map(LlmEvent::TextDelta))
            }
            LlmEvent::ReasoningDelta { text, kind } => {
                let forwarded = skip_validated_prefix(
                    text,
                    &skip.emitted_reasoning,
                    &mut self.seen_reasoning_chars,
                )?;
                Ok(forwarded.map(|text| LlmEvent::ReasoningDelta { text, kind }))
            }
            LlmEvent::ReasoningDone(payload) => {
                self.seen_reasoning_done += 1;
                if self.seen_reasoning_done <= skip.emitted_reasoning_done {
                    Ok(None)
                } else {
                    Ok(Some(LlmEvent::ReasoningDone(payload)))
                }
            }
            LlmEvent::ToolCall(call) => {
                let index = self.seen_tool_calls;
                self.seen_tool_calls += 1;
                match skip.emitted_tool_call_ids.get(index) {
                    Some(expected) if *expected == call.call_id => Ok(None),
                    Some(expected) => Err(SqueezyError::ProviderStream(format!(
                        "stream reconnect diverged: tool call #{index} regenerated as \
                         {:?}, but {expected:?} was already emitted",
                        call.call_id,
                    ))),
                    None => Ok(Some(LlmEvent::ToolCall(call))),
                }
            }
            LlmEvent::Completed {
                response_id,
                cost,
                stop_reason,
                reasoning_only_stop,
            } => Ok(Some(LlmEvent::Completed {
                response_id,
                cost,
                stop_reason,
                reasoning_only_stop,
            })),
            LlmEvent::Cancelled => Ok(Some(LlmEvent::Cancelled)),
            LlmEvent::ContextOverflow { provider, signal } => {
                Ok(Some(LlmEvent::ContextOverflow { provider, signal }))
            }
            LlmEvent::ServerModel(model) => {
                if skip.emitted_server_model {
                    Ok(None)
                } else {
                    Ok(Some(LlmEvent::ServerModel(model)))
                }
            }
            other => {
                debug_assert!(
                    !matches!(other, LlmEvent::ToolCallDelta { .. }),
                    "with_stream_retry forwards ToolCallDelta without per-call_id skip \
                     accounting; a reconnect would replay its prefix. Extend SkipCursor / \
                     StreamSkipState before routing a ToolCallDelta-emitting provider through \
                     with_stream_retry.",
                );
                Ok(Some(other))
            }
        }
    }

    /// Detects a regenerated attempt that completes before reproducing the
    /// full already-emitted prefix (shorter text/reasoning, or fewer tool
    /// calls). Such a truncation would leave the caller holding the prior
    /// attempt's longer prefix glued to this shorter generation's
    /// `Completed`, so we surface it as a stream error.
    fn check_complete(&self) -> Result<()> {
        if self.seen_text_chars < self.expected_text_chars {
            return Err(SqueezyError::ProviderStream(
                "stream reconnect diverged: regenerated text shorter than the \
                 already-emitted prefix"
                    .to_string(),
            ));
        }
        if self.seen_reasoning_chars < self.expected_reasoning_chars {
            return Err(SqueezyError::ProviderStream(
                "stream reconnect diverged: regenerated reasoning shorter than the \
                 already-emitted prefix"
                    .to_string(),
            ));
        }
        if self.seen_tool_calls < self.expected_tool_calls {
            return Err(SqueezyError::ProviderStream(
                "stream reconnect diverged: regenerated attempt omitted an \
                 already-emitted tool call"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

/// Splits `text` at the `skip_chars`-th character boundary, returning the
/// number of leading chars that fall inside the skip window (`min(skip_chars,
/// total_chars)`) and the not-yet-seen suffix (`None` when the whole delta is
/// inside the window). Borrows `text` so the caller can still validate the
/// suppressed prefix against the recorded content before discarding it.
pub(crate) fn split_delta_prefix(text: &str, skip_chars: usize) -> (usize, Option<&str>) {
    if text.is_empty() {
        return (0, None);
    }
    if skip_chars == 0 {
        return (0, Some(text));
    }

    let mut total_chars = 0usize;
    let mut split_at = None;
    for (byte_index, _) in text.char_indices() {
        if total_chars == skip_chars {
            split_at = Some(byte_index);
        }
        total_chars += 1;
    }
    if skip_chars >= total_chars {
        return (total_chars, None);
    }

    let suffix = &text[split_at.expect("split point exists when skip_chars < chars")..];
    (skip_chars, Some(suffix))
}

/// Suppress the portion of `text` that re-covers the already-emitted `prefix`,
/// forwarding only the not-yet-seen suffix. The skipped chars must match
/// `prefix` verbatim; a mismatch means the regenerated stream diverged from
/// the prefix the caller already observed, which we refuse to splice. `seen`
/// tracks how many `prefix` chars this attempt has reproduced so far and is
/// advanced by the number of chars this delta covered.
fn skip_validated_prefix(text: String, prefix: &str, seen: &mut usize) -> Result<Option<String>> {
    let already = prefix.chars().count().saturating_sub(*seen);
    let (consumed, forwarded) = split_delta_prefix(&text, already);
    let regenerated = text.chars().take(consumed);
    if !prefix.chars().skip(*seen).take(consumed).eq(regenerated) {
        return Err(SqueezyError::ProviderStream(format!(
            "stream reconnect diverged: regenerated text {:?} does not match the \
             already-emitted prefix {:?}",
            text.chars().take(consumed).collect::<String>(),
            prefix
                .chars()
                .skip(*seen)
                .take(consumed)
                .collect::<String>(),
        )));
    }
    *seen += consumed + forwarded.map_or(0, |suffix| suffix.chars().count());
    Ok(forwarded.map(str::to_string))
}

/// Wraps a stream-producing closure so transient mid-stream errors trigger a
/// reconnect bounded by `policy.max_retries`. Already-yielded events are
/// tracked via [`StreamSkipState`] so a fresh attempt only emits the suffix
/// the caller has not yet observed. A `tracing` event is recorded on every
/// reconnect under `target = "squeezy_llm::stream_retry"` carrying
/// `provider` and `attempt` fields.
pub fn with_stream_retry<F>(
    provider: &'static str,
    policy: RetryPolicy,
    cancel: CancellationToken,
    mut make_attempt: F,
) -> LlmStream
where
    F: FnMut() -> LlmStream + Send + 'static,
{
    let stream = try_stream! {
        let mut skip = StreamSkipState::default();
        let mut attempt: u8 = 0;
        loop {
            let mut cursor = SkipCursor::for_attempt(&skip);
            let mut inner = make_attempt();
            let mut transient_error: Option<SqueezyError> = None;
            let mut completed = false;
            'inner: loop {
                let next = tokio::select! {
                    _ = cancel.cancelled() => {
                        yield LlmEvent::Cancelled;
                        return;
                    }
                    next = inner.next() => next,
                };
                match next {
                    None => break 'inner,
                    Some(Ok(event)) => {
                        let was_completed = matches!(event, LlmEvent::Completed { .. });
                        if was_completed
                            && let Err(err) = cursor.check_complete()
                        {
                            if is_reconnect_divergence(&err) && skip.is_uncommitted() {
                                skip.reset_content_for_restart();
                                transient_error = Some(err);
                                break 'inner;
                            }
                            Err(err)?;
                        }
                        match cursor.filter(event, &skip) {
                            Ok(maybe_forwarded) => {
                                if let Some(forwarded) = maybe_forwarded {
                                    skip.observe_yielded(&forwarded);
                                    yield forwarded;
                                }
                            }
                            Err(err) => {
                                if is_reconnect_divergence(&err) && skip.is_uncommitted() {
                                    skip.reset_content_for_restart();
                                    transient_error = Some(err);
                                    break 'inner;
                                }
                                Err(err)?;
                            }
                        }
                        if was_completed {
                            completed = true;
                            break 'inner;
                        }
                    }
                    Some(Err(err)) => {
                        if is_retryable_stream_error(&err) {
                            transient_error = Some(err);
                            break 'inner;
                        }
                        Err(err)?;
                        unreachable!("stream error returned above");
                    }
                }
            }

            if completed {
                return;
            }

            let Some(err) = transient_error else {
                if attempt >= policy.max_retries {
                    Err(SqueezyError::ProviderStream(
                        "provider stream ended without completion".to_string(),
                    ))?;
                    unreachable!("returned above");
                }
                attempt += 1;
                tracing::warn!(
                    target: "squeezy_llm::stream_retry",
                    provider,
                    attempt,
                    max = policy.max_retries,
                    "provider stream truncated; reconnecting",
                );
                sleep_or_cancel(&cancel, capped_backoff(policy, attempt - 1)).await?;
                continue;
            };

            if attempt >= policy.max_retries {
                Err(err)?;
                unreachable!("returned above");
            }
            attempt += 1;
            tracing::warn!(
                target: "squeezy_llm::stream_retry",
                provider,
                attempt,
                max = policy.max_retries,
                error = %err,
                "provider stream error; reconnecting",
            );
            sleep_or_cancel(&cancel, capped_backoff(policy, attempt - 1)).await?;
        }
    };
    Box::pin(stream)
}
