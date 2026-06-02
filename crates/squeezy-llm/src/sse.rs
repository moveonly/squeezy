//! Server-Sent Events decoder shared by every provider client that streams
//! over SSE (OpenAI Responses, OpenAI-compatible Chat Completions, Google
//! Gemini, Anthropic Messages). Each provider parses the `data:` payload
//! itself; this module only frames the byte stream into individual events.

/// A decoded SSE event: the joined `data:` payload alongside the
/// optional `event:` name. Most providers only consume the `data:`
/// payload (and call [`SseDecoder::push`] / [`SseDecoder::finish`]),
/// but the `event:` field is preserved here so future call sites that
/// need stream-phase disambiguation can switch to
/// [`SseDecoder::push_with_events`] / [`SseDecoder::finish_with_events`]
/// without another decoder refactor (L3).
///
/// Note: the `event:` parsing here is forward-looking scaffolding, not
/// the mechanism behind C-01 (mid-stream provider errors). Anthropic's
/// mid-stream error surfaces through the *JSON* `"type":"error"` branch
/// of the per-provider `data:` parser fed by [`SseDecoder::push`]; it
/// does not depend on the SSE `event:` line at all. No production caller
/// reads this field today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub data: String,
    pub event: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
    /// Byte offset into `buffer` where the next boundary scan should
    /// resume. Without this, every push re-scans the entire buffer with
    /// `.windows(2)` — O(n²) on multi-MB reasoning streams where a
    /// single event can span many push calls before the `\n\n`
    /// terminator arrives.
    scan_pos: usize,
}

impl SseDecoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.push_with_events(bytes)
            .into_iter()
            .map(|e| e.data)
            .collect()
    }

    pub(crate) fn finish(&mut self) -> Vec<String> {
        self.finish_with_events()
            .into_iter()
            .map(|e| e.data)
            .collect()
    }

    /// Like [`Self::push`] but preserves the `event:` field on each
    /// returned event. The `data` payload is identical to what
    /// [`Self::push`] would produce. No production caller reads
    /// `event` yet (see L3 audit note in `.audit/providers/openai-compatible.md`);
    /// when one needs it (Cohere, future OpenAI Responses Mux phases,
    /// MCP server-streaming), swap the call site over without
    /// changing this module.
    // TODO(L3): migrate `google.rs`, `compatible.rs`, `lmstudio.rs`,
    // `openai.rs`, `oauth/openai_codex.rs`, `anthropic.rs` to
    // `push_with_events` if/when any of them needs the `event:` name.
    pub(crate) fn push_with_events(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();

        loop {
            match find_event_boundary(&self.buffer, self.scan_pos) {
                Some((index, len)) => {
                    let event = self.buffer.drain(..index + len).collect::<Vec<_>>();
                    // Drained the entire prefix the scanner had walked
                    // (the boundary itself is part of that prefix), so
                    // resume from byte 0 of the now-shorter buffer.
                    self.scan_pos = 0;
                    events.extend(decode_sse_event(&event));
                }
                None => {
                    // No boundary yet. Park the cursor near the tail so
                    // the next push only scans newly-appended bytes.
                    // Keep a 3-byte overlap so a `\r\n\r\n` boundary
                    // straddling the push gap is still caught.
                    self.scan_pos = self.buffer.len().saturating_sub(3);
                    break;
                }
            }
        }

        events
    }

    pub(crate) fn finish_with_events(&mut self) -> Vec<SseEvent> {
        self.scan_pos = 0;
        if self.buffer.is_empty() {
            return Vec::new();
        }

        let event = std::mem::take(&mut self.buffer);
        decode_sse_event(&event)
    }
}

fn find_event_boundary(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    let start = start.min(bytes.len());
    let tail = &bytes[start..];
    let lf = tail
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (start + index, 2));
    let crlf = tail
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (start + index, 4));

    [lf, crlf].into_iter().flatten().min_by_key(|b| b.0)
}

fn decode_sse_event(bytes: &[u8]) -> Vec<SseEvent> {
    const DONE_SENTINEL: &str = "[DONE]";
    let text = String::from_utf8_lossy(bytes);
    let mut events: Vec<SseEvent> = Vec::new();
    let mut pending: Vec<&str> = Vec::new();
    // L3: SSE allows an `event: <name>` line per event. Capture the
    // last such name in the frame; it applies to every payload that
    // the frame surfaces (including any post-`[DONE]` split).
    //
    // Caveat: this is *last-wins per frame* and is stamped onto every
    // payload from the frame at flush time — including any `data:`
    // line that physically preceded the `event:` line within the same
    // frame. A strict per-event reader would tie each name only to the
    // payload it introduces. The field is unused in production (see the
    // module-level note), so the simplification is harmless; tighten it
    // to per-event association before any consumer relies on the name.
    let mut event_name: Option<String> = None;
    let push_pending =
        |events: &mut Vec<SseEvent>, pending: &mut Vec<&str>, name: &Option<String>| {
            if !pending.is_empty() {
                events.push(SseEvent {
                    data: pending.join("\n"),
                    event: name.clone(),
                });
                pending.clear();
            }
        };
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(name) = line.strip_prefix("event:") {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                event_name = Some(trimmed.to_string());
            }
        } else if let Some(data) = line.strip_prefix("data:") {
            // SSE spec (WHATWG EventSource §9.2) allows empty `data:`
            // lines as keep-alive padding. Some providers (notably OpenAI
            // on long reasoning turns) emit them between real chunks;
            // forwarding `""` to `serde_json::from_str` crashes the
            // stream. Drop empties; also tolerate trailing whitespace
            // around the `[DONE]` sentinel (some providers send
            // `data: [DONE] \n`).
            //
            // Spec deviation: WHATWG EventSource §9.2 strips only a
            // single leading space after the colon (`data: x` -> `x`,
            // but `data:  x` -> ` x`) and never touches the trailing
            // edge. We `trim()` both ends instead. This is benign for
            // every current consumer because each parses the payload as
            // JSON or compares it against the `[DONE]` sentinel — both
            // of which ignore surrounding ASCII whitespace. If a future
            // consumer needs whitespace-significant payloads, narrow
            // this to `data.strip_prefix(' ').unwrap_or(data)` (single
            // leading space, no trailing strip) to match the spec.
            let payload = data.trim();
            if payload.is_empty() {
                continue;
            }
            // L4: if a provider mis-frames the terminal `[DONE]`
            // sentinel into the same SSE event as a preceding JSON
            // payload (e.g. `data: {usage:...}\ndata: [DONE]\n\n`),
            // splitting on the data-line boundary lets the JSON parse
            // before the sentinel surfaces. Outside this case, multi
            // `data:` lines are still joined with `\n` per the SSE
            // spec.
            if payload == DONE_SENTINEL {
                push_pending(&mut events, &mut pending, &event_name);
                events.push(SseEvent {
                    data: payload.to_string(),
                    event: event_name.clone(),
                });
            } else {
                pending.push(payload);
            }
        }
    }
    push_pending(&mut events, &mut pending, &event_name);
    events
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;
