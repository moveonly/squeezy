use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader},
    path::Path,
};

use serde_json::Value;
use sha2::{Digest, Sha256};
use squeezy_core::{ReasoningSnapshot, Result, TranscriptItem};

use super::{
    HydratedToolCall, HydratedTranscriptItem, ResumeItem, SESSION_REPLAY_SCHEMA_VERSION,
    SessionEvent, SessionEventKind, SessionReplayEvent,
};

/// Per-replay state that survives across `apply_event_to_replay`
/// calls. Carries:
///
/// - `pending_reasoning`: provider streams emit `Reasoning` events
///   independently of the assistant message that follows. The
///   rendered transcript only has a place to attach a reasoning
///   snapshot via `TranscriptItem.reasoning` on the assistant
///   message itself, so we buffer reasoning here until the next
///   `AssistantCompleted` drains it.
/// - `pending_tool_calls`: tool-result hydration needs the matching
///   `ToolCall` (for the tool name + arguments) to rebuild the
///   transcript card. The agent emits `ToolCall` and `ToolResult`
///   as separate events linked by `call_id`; we hash the call by
///   id when we see it and look it up when the result lands.
///
/// Without this buffering, resume drops every reasoning chip and
/// every tool-result card the original turn produced -- what the
/// LLM had in its context, the user did not see on screen.
#[derive(Debug, Default)]
pub(super) struct ReplayState {
    pending_reasoning: Vec<ReasoningSnapshot>,
    pending_tool_calls: HashMap<String, HydratedToolCall>,
}

impl ReplayState {
    fn drain_combined_reasoning(&mut self) -> Option<ReasoningSnapshot> {
        if self.pending_reasoning.is_empty() {
            return None;
        }
        // Concatenate every buffered segment's display text so the
        // resumed chip carries the full reasoning the user originally
        // watched stream in, separated by blank lines so a reviewer
        // can still see the segment boundaries. The payload comes
        // from the last segment so per-provider metadata (item_id,
        // encrypted_content, thought_signature) stays consistent
        // with what the provider would return on a fresh turn.
        let mut display = String::new();
        for snap in &self.pending_reasoning {
            if !display.is_empty() {
                display.push_str("\n\n");
            }
            display.push_str(&snap.display_text);
        }
        let last_payload = self
            .pending_reasoning
            .last()
            .map(|s| s.payload.clone())
            .expect("non-empty");
        self.pending_reasoning.clear();
        Some(ReasoningSnapshot {
            display_text: display,
            payload: last_payload,
        })
    }
}

pub(super) fn apply_event_to_replay(
    event: &SessionEvent,
    conversation: &mut Vec<ResumeItem>,
    transcript: &mut Vec<TranscriptItem>,
    hydrated: &mut Vec<HydratedTranscriptItem>,
    replay: &mut ReplayState,
) {
    let Some(typed) = SessionEventKind::try_from_event(event) else {
        return;
    };
    match typed {
        SessionEventKind::UserMessage { text } => {
            conversation.push(ResumeItem::UserText { text: text.clone() });
            let item = TranscriptItem::user(text);
            transcript.push(item.clone());
            hydrated.push(HydratedTranscriptItem::Message { item });
        }
        SessionEventKind::AssistantCompleted { text, .. } => {
            if text.is_empty() {
                return;
            }
            conversation.push(ResumeItem::AssistantText { text: text.clone() });
            // Drain any reasoning buffered since the last assistant
            // message and attach it to this one so the resumed
            // transcript shows the reasoning chip via the
            // `format_assistant_message_entry` embedded-chip path.
            let attached = replay.drain_combined_reasoning();
            let item = TranscriptItem::assistant_with_reasoning(text, attached);
            transcript.push(item.clone());
            hydrated.push(HydratedTranscriptItem::Message { item });
        }
        SessionEventKind::ToolCall {
            call_id,
            tool,
            arguments,
        } => {
            if call_id.is_empty() {
                return;
            }
            // Buffer for the matching ToolResult event so the
            // hydrated transcript can carry the call's name + args
            // alongside the result body -- without it the resumed
            // card has no tool label and no command preview.
            replay.pending_tool_calls.insert(
                call_id.clone(),
                HydratedToolCall {
                    call_id: call_id.clone(),
                    tool: tool.clone(),
                    arguments: arguments.clone(),
                },
            );
            conversation.push(ResumeItem::FunctionCall {
                call_id,
                name: tool,
                arguments,
            });
        }
        SessionEventKind::ToolResult { output } => {
            let Some(call_id) = output.get("call_id").and_then(Value::as_str) else {
                return;
            };
            let call_id_owned = call_id.to_string();
            let body = output
                .get("output")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| output.to_string());
            conversation.push(ResumeItem::FunctionCallOutput {
                call_id: call_id_owned.clone(),
                output: body,
            });
            // Pair with the buffered call (if we saw one) so the
            // TUI can rebuild a full tool-result card on hydration.
            // A missing call is rare -- the agent always writes
            // `ToolCall` before `ToolResult` for the same id -- but
            // we still record the result so a resumed session
            // doesn't silently drop tool output entirely.
            let call = replay.pending_tool_calls.remove(&call_id_owned);
            hydrated.push(HydratedTranscriptItem::ToolResult {
                call,
                result: output,
            });
        }
        // Compaction events are handled by the snap-to-checkpoint path in
        // `replay_resume_state`; appearing here means a checkpoint with
        // no `conversation` field, so we treat it as a no-op and let the
        // linear replay continue.
        SessionEventKind::ContextCompacted { .. } => {}
        SessionEventKind::Reasoning { payload } => {
            conversation.push(ResumeItem::Reasoning {
                payload: payload.clone(),
            });
            replay
                .pending_reasoning
                .push(ReasoningSnapshot::from_payload(payload));
        }
        // Approval and session-lifecycle events are bookkeeping rather
        // than conversation items; they do not modify the resume state's
        // conversation/transcript but still need to be enumerated so the
        // match is exhaustive (catches future kinds at compile time).
        // Custom events are extension-authored sidecar data -- core
        // replay must ignore them so an extension cannot corrupt the
        // reconstructed conversation by writing arbitrary payloads.
        SessionEventKind::ApprovalRequested { .. }
        | SessionEventKind::ApprovalDecided { .. }
        | SessionEventKind::SessionStarted
        | SessionEventKind::SessionEnded { .. }
        | SessionEventKind::Cancelled
        | SessionEventKind::Failed { .. }
        | SessionEventKind::SessionResumed
        | SessionEventKind::Custom { .. }
        | SessionEventKind::Unknown => {}
    }
}

pub(super) fn read_replay_jsonl(path: &Path) -> Result<(Vec<SessionReplayEvent>, u64)> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(error) => return Err(error.into()),
    };
    let mut events = Vec::new();
    let mut warnings = 0;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match parse_replay_jsonl_line(trimmed) {
            Some(event) => events.push(event),
            None => warnings += 1,
        }
    }
    Ok((events, warnings))
}

pub(super) fn count_replay_jsonl(path: &Path) -> Result<(u64, u64)> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(error) => return Err(error.into()),
    };
    let mut events = 0;
    let mut warnings = 0;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if parse_replay_jsonl_line(trimmed).is_some() {
            events += 1;
        } else {
            warnings += 1;
        }
    }
    Ok((events, warnings))
}

fn parse_replay_jsonl_line(line: &str) -> Option<SessionReplayEvent> {
    let event = serde_json::from_str::<SessionReplayEvent>(line).ok()?;
    if event.schema_version == SESSION_REPLAY_SCHEMA_VERSION
        && event.payload_sha256 == replay_payload_sha256(&event.payload)
    {
        Some(event)
    } else {
        None
    }
}

pub(super) fn replay_payload_sha256(payload: &Value) -> String {
    use std::fmt::Write as _;

    let bytes = serde_json::to_vec(payload).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
