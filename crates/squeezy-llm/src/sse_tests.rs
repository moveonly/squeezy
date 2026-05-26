use super::SseDecoder;

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
