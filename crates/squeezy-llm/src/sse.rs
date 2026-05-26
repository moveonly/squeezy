//! Server-Sent Events decoder shared by every provider client that streams
//! over SSE (OpenAI Responses, OpenAI-compatible Chat Completions, Google
//! Gemini, Anthropic Messages). Each provider parses the `data:` payload
//! itself; this module only frames the byte stream into individual events.

#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();

        while let Some((index, len)) = find_event_boundary(&self.buffer) {
            let event = self.buffer.drain(..index + len).collect::<Vec<_>>();
            if let Some(data) = decode_sse_event(&event) {
                events.push(data);
            }
        }

        events
    }

    pub(crate) fn finish(&mut self) -> Vec<String> {
        if self.buffer.is_empty() {
            return Vec::new();
        }

        let event = std::mem::take(&mut self.buffer);
        decode_sse_event(&event).into_iter().collect()
    }
}

fn find_event_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2));
    let crlf = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4));

    [lf, crlf].into_iter().flatten().min_by_key(|b| b.0)
}

fn decode_sse_event(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut data_lines = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start());
        }
    }
    if data_lines.is_empty() {
        None
    } else {
        Some(data_lines.join("\n"))
    }
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;
