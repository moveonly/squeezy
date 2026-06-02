use super::{SseDecoder, SseEvent};

#[test]
fn splits_single_event() {
    let mut decoder = SseDecoder::default();
    let events = decoder.push(b"data: hello\n\n");
    assert_eq!(events, vec!["hello".to_string()]);
    assert!(decoder.finish().is_empty());
}

#[test]
fn splits_event_across_pushes() {
    let mut decoder = SseDecoder::default();
    assert!(decoder.push(b"data: hel").is_empty());
    assert!(decoder.push(b"lo").is_empty());
    let events = decoder.push(b"\n\n");
    assert_eq!(events, vec!["hello".to_string()]);
}

#[test]
fn joins_multiple_data_lines() {
    let mut decoder = SseDecoder::default();
    let events = decoder.push(b"data: line one\ndata: line two\n\n");
    assert_eq!(events, vec!["line one\nline two".to_string()]);
}

#[test]
fn ignores_comment_and_blank_lines() {
    let mut decoder = SseDecoder::default();
    let events = decoder.push(b": heartbeat\nevent: ping\ndata: payload\n\n");
    assert_eq!(events, vec!["payload".to_string()]);
}

#[test]
fn supports_crlf_boundaries() {
    let mut decoder = SseDecoder::default();
    let events = decoder.push(b"data: alpha\r\n\r\ndata: beta\r\n\r\n");
    assert_eq!(events, vec!["alpha".to_string(), "beta".to_string()]);
}

#[test]
fn returns_multiple_events_from_single_push() {
    let mut decoder = SseDecoder::default();
    let events = decoder.push(b"data: one\n\ndata: two\n\ndata: three\n\n");
    assert_eq!(
        events,
        vec!["one".to_string(), "two".to_string(), "three".to_string()],
    );
}

#[test]
fn finish_flushes_trailing_event_without_terminator() {
    let mut decoder = SseDecoder::default();
    assert!(decoder.push(b"data: dangling").is_empty());
    let events = decoder.finish();
    assert_eq!(events, vec!["dangling".to_string()]);
}

#[test]
fn finish_drops_buffer_with_no_data_lines() {
    let mut decoder = SseDecoder::default();
    assert!(decoder.push(b": just-a-comment").is_empty());
    assert!(decoder.finish().is_empty());
}

#[test]
fn decode_drops_empty_data_lines() {
    // X-02: WHATWG EventSource §9.2 allows empty `data:` heartbeats.
    // OpenAI emits them on long reasoning turns; forwarding `""` to
    // `serde_json::from_str` would crash the stream with EOF.
    let mut decoder = SseDecoder::default();
    let events = decoder.push(b"data:\n\n");
    assert!(
        events.is_empty(),
        "empty `data:` heartbeat must not surface as an event"
    );
}

#[test]
fn decode_drops_whitespace_only_data_lines() {
    // X-02: `data:   \n\n` (only whitespace) is still a heartbeat.
    let mut decoder = SseDecoder::default();
    let events = decoder.push(b"data:    \n\n");
    assert!(events.is_empty());
}

#[test]
fn decode_keeps_payload_when_only_some_data_lines_empty() {
    // X-02: a multi-`data:` event with one empty line should still yield
    // the non-empty payload (not be dropped as fully empty).
    let mut decoder = SseDecoder::default();
    let events = decoder.push(b"data: real\ndata:\n\n");
    assert_eq!(events, vec!["real".to_string()]);
}

#[test]
fn decode_trims_whitespace_around_done_sentinel() {
    // X-02: providers like Together / vLLM occasionally emit
    // `data: [DONE] \n\n` (trailing space). Downstream `[DONE]` literal
    // comparisons must match after trim.
    let mut decoder = SseDecoder::default();
    let events = decoder.push(b"data: [DONE] \n\n");
    assert_eq!(events, vec!["[DONE]".to_string()]);
}

#[test]
fn find_event_boundary_is_linear_across_pushes() {
    // L2: many small pushes without a terminator must not re-scan the
    // whole buffer each time. 50k 64-byte chunks of a single
    // un-terminated `data:` line, then close. The O(n^2) version
    // re-scanned the whole (growing) buffer on every push — ~50k * (50k
    // * 64 / 2) ≈ 8e10 byte comparisons, which ran for many seconds even
    // on fast hardware; the linear version scans each appended byte once
    // (~3.2e6 comparisons) and finishes in milliseconds.
    //
    // The functional assertions below (one event, full length preserved)
    // are the primary contract. The wall-clock bound is a *coarse*
    // backstop for the quadratic regression and is deliberately generous
    // (60s) so a loaded / oversubscribed CI runner does not flake: the
    // quadratic version is orders of magnitude slower than that, while
    // the linear version stays four-plus orders under it. Treat the
    // timing assertion as advisory — if it ever flakes, raise the bound
    // rather than weakening the count/length checks.
    let chunk = vec![b'x'; 64];
    let mut decoder = SseDecoder::default();
    decoder.push(b"data: ");
    let start = std::time::Instant::now();
    for _ in 0..50_000 {
        assert!(decoder.push(&chunk).is_empty());
    }
    let elapsed = start.elapsed();
    // Sanity: terminating the event still yields exactly one payload of
    // the full accumulated length — this is the assertion that actually
    // proves the scan-position bookkeeping is correct.
    let events = decoder.push(b"\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].len(), 50_000 * 64);
    assert!(
        elapsed < std::time::Duration::from_secs(60),
        "boundary scan should be linear; took {elapsed:?} for 50k pushes (a quadratic \
         regression would run for many multiples of this bound)",
    );
}

#[test]
fn crlf_boundary_split_across_pushes() {
    // L2: scan-position overlap must preserve the ability to detect a
    // `\r\n\r\n` boundary that straddles two pushes.
    let mut decoder = SseDecoder::default();
    assert!(decoder.push(b"data: hi\r\n\r").is_empty());
    let events = decoder.push(b"\n");
    assert_eq!(events, vec!["hi".to_string()]);
}

#[test]
fn lf_boundary_split_across_pushes() {
    // L2: same as above for the simpler `\n\n` boundary.
    let mut decoder = SseDecoder::default();
    assert!(decoder.push(b"data: hi\n").is_empty());
    let events = decoder.push(b"\n");
    assert_eq!(events, vec!["hi".to_string()]);
}

#[test]
fn done_joined_with_prior_json() {
    // L4: providers occasionally mis-frame `[DONE]` into the same SSE
    // event as the preceding JSON payload (`data: {usage:...}\ndata:
    // [DONE]\n\n`). Joining both with `\n` produces invalid JSON that
    // crashes the chat-completions parser. Split into two events so
    // the JSON parses cleanly before the sentinel surfaces.
    let mut decoder = SseDecoder::default();
    let events = decoder.push(b"data: {\"usage\":{\"total_tokens\":42}}\ndata: [DONE]\n\n");
    assert_eq!(
        events,
        vec![
            "{\"usage\":{\"total_tokens\":42}}".to_string(),
            "[DONE]".to_string(),
        ],
        "JSON payload and [DONE] must arrive as separate events"
    );
}

#[test]
fn done_alone_remains_a_single_event() {
    // L4: the common shape `data: [DONE]\n\n` must still yield exactly
    // one event so downstream consumers don't see a spurious empty
    // payload before the sentinel.
    let mut decoder = SseDecoder::default();
    let events = decoder.push(b"data: [DONE]\n\n");
    assert_eq!(events, vec!["[DONE]".to_string()]);
}

#[test]
fn multiple_data_lines_still_joined_when_no_done() {
    // L4: ordinary multi-`data:` events must still be joined with `\n`
    // per the SSE spec. The split rule applies only when the literal
    // `[DONE]` sentinel is present in the frame.
    let mut decoder = SseDecoder::default();
    let events = decoder.push(b"data: first\ndata: second\ndata: third\n\n");
    assert_eq!(events, vec!["first\nsecond\nthird".to_string()]);
}

#[test]
fn event_field_preserved() {
    // L3: `push_with_events` must surface the `event:` field. The
    // backward-compat `push` projection drops it; both APIs must agree
    // on `data`.
    let mut decoder = SseDecoder::default();
    let events = decoder.push_with_events(b"event: response.refusal.delta\ndata: payload\n\n");
    assert_eq!(
        events,
        vec![SseEvent {
            data: "payload".to_string(),
            event: Some("response.refusal.delta".to_string()),
        }]
    );
}

#[test]
fn event_field_absent_is_none() {
    // L3: a frame with no `event:` line yields `event: None` on the
    // returned struct.
    let mut decoder = SseDecoder::default();
    let events = decoder.push_with_events(b"data: payload\n\n");
    assert_eq!(
        events,
        vec![SseEvent {
            data: "payload".to_string(),
            event: None,
        }]
    );
}

#[test]
fn event_field_empty_value_treated_as_none() {
    // L3: an empty `event:` line (only whitespace) should not latch a
    // bogus phase name.
    let mut decoder = SseDecoder::default();
    let events = decoder.push_with_events(b"event:   \ndata: payload\n\n");
    assert_eq!(
        events,
        vec![SseEvent {
            data: "payload".to_string(),
            event: None,
        }]
    );
}

#[test]
fn event_field_applies_to_split_payloads() {
    // L3 + L4: when a frame is split because of the `[DONE]` rule, the
    // captured `event:` name must apply to every surfaced payload.
    let mut decoder = SseDecoder::default();
    let events = decoder
        .push_with_events(b"event: response.completed\ndata: {\"ok\":true}\ndata: [DONE]\n\n");
    assert_eq!(
        events,
        vec![
            SseEvent {
                data: "{\"ok\":true}".to_string(),
                event: Some("response.completed".to_string()),
            },
            SseEvent {
                data: "[DONE]".to_string(),
                event: Some("response.completed".to_string()),
            },
        ]
    );
}

#[test]
fn push_projection_drops_event_field_for_legacy_callers() {
    // L3: the backward-compatible `push` API still returns only the
    // `data` payload so existing callers (`google.rs`, `compatible.rs`,
    // etc.) compile unchanged.
    let mut decoder = SseDecoder::default();
    let events = decoder.push(b"event: response.completed\ndata: payload\n\n");
    assert_eq!(events, vec!["payload".to_string()]);
}
