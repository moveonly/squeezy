use super::*;

#[test]
fn decoder_parses_event_and_data_lines() {
    let mut decoder = SseDecoder::default();
    decoder
        .feed(b"event: endpoint\ndata: /messages?session=abc\n\n")
        .expect("feed ok");
    let frame = decoder.pop().expect("frame ready");
    assert_eq!(frame.event.as_deref(), Some("endpoint"));
    assert_eq!(frame.data.as_deref(), Some("/messages?session=abc"));
}

#[test]
fn decoder_joins_multi_line_data() {
    let mut decoder = SseDecoder::default();
    decoder
        .feed(b"event: message\ndata: line1\ndata: line2\n\n")
        .expect("feed ok");
    let frame = decoder.pop().expect("frame ready");
    assert_eq!(frame.event.as_deref(), Some("message"));
    assert_eq!(frame.data.as_deref(), Some("line1\nline2"));
}

#[test]
fn decoder_handles_chunked_byte_arrivals() {
    let mut decoder = SseDecoder::default();
    for chunk in [
        b"event:".as_ref(),
        b" endpoint\nda".as_ref(),
        b"ta: /sess".as_ref(),
        b"ion-1\n\n".as_ref(),
    ] {
        decoder.feed(chunk).expect("feed ok");
    }
    let frame = decoder.pop().expect("frame ready");
    assert_eq!(frame.event.as_deref(), Some("endpoint"));
    assert_eq!(frame.data.as_deref(), Some("/session-1"));
}

#[test]
fn decoder_treats_blank_event_as_message() {
    assert!(is_message_event(None));
    assert!(is_message_event(Some("")));
    assert!(is_message_event(Some("message")));
    assert!(!is_message_event(Some("endpoint")));
    assert!(!is_message_event(Some("ping")));
}

#[test]
fn decoder_skips_comment_lines() {
    let mut decoder = SseDecoder::default();
    decoder
        .feed(b": this is a comment\nevent: message\ndata: hi\n\n")
        .expect("feed ok");
    let frame = decoder.pop().expect("frame ready");
    assert_eq!(frame.event.as_deref(), Some("message"));
    assert_eq!(frame.data.as_deref(), Some("hi"));
}

#[test]
fn decoder_handles_crlf_line_endings() {
    let mut decoder = SseDecoder::default();
    decoder
        .feed(b"event: message\r\ndata: hi\r\n\r\n")
        .expect("feed ok");
    let frame = decoder.pop().expect("frame ready");
    assert_eq!(frame.event.as_deref(), Some("message"));
    assert_eq!(frame.data.as_deref(), Some("hi"));
}

#[test]
fn resolve_endpoint_url_joins_relative_path() {
    let joined =
        resolve_endpoint_url("https://example.test/sse", "/messages?sid=abc").expect("resolves");
    assert_eq!(joined, "https://example.test/messages?sid=abc");
}

#[test]
fn resolve_endpoint_url_preserves_absolute_url() {
    let joined = resolve_endpoint_url("https://example.test/sse", "https://other.test/post")
        .expect("resolves");
    assert_eq!(joined, "https://other.test/post");
}

#[test]
fn resolve_endpoint_url_rejects_empty() {
    let err = resolve_endpoint_url("https://example.test/sse", "").expect_err("empty rejected");
    assert!(matches!(err, SseTransportError::InvalidUrl { .. }));
}
