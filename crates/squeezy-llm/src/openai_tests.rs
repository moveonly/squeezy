use super::*;

#[test]
fn request_body_uses_responses_streaming_shape() {
    let request = LlmRequest {
        model: "gpt-test".to_string(),
        instructions: "be brief".to_string(),
        input: "hello".to_string(),
        max_output_tokens: Some(32),
        previous_response_id: Some("resp_123".to_string()),
    };

    let body = OpenAiProvider::request_body(&request);

    assert_eq!(body["model"], "gpt-test");
    assert_eq!(body["instructions"], "be brief");
    assert_eq!(body["input"], "hello");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["max_output_tokens"], 32);
    assert_eq!(body["previous_response_id"], "resp_123");
}

#[test]
fn sse_decoder_collects_data_events_across_chunks() {
    let mut decoder = SseDecoder::default();

    assert!(
        decoder
            .push(b"event: message\ndata: {\"type\":\"response.")
            .is_empty()
    );
    let events = decoder.push(b"output_text.delta\",\"delta\":\"hi\"}\n\n");

    assert_eq!(
        events,
        vec![r#"{"type":"response.output_text.delta","delta":"hi"}"#]
    );
}

#[test]
fn sse_decoder_accepts_crlf_event_delimiters() {
    let mut decoder = SseDecoder::default();
    let events = decoder.push(
        b"event: message\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\r\n\r\n",
    );

    assert_eq!(
        events,
        vec![r#"{"type":"response.output_text.delta","delta":"hi"}"#]
    );
}

#[test]
fn sse_decoder_splits_multiple_crlf_events() {
    let mut decoder = SseDecoder::default();
    let events = decoder.push(
        b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"one\"}\r\n\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"two\"}\r\n\r\n",
    );

    assert_eq!(
        events,
        vec![
            r#"{"type":"response.output_text.delta","delta":"one"}"#,
            r#"{"type":"response.output_text.delta","delta":"two"}"#
        ]
    );
}

#[test]
fn parser_extracts_text_delta() {
    let event = parse_openai_event(r#"{"type":"response.output_text.delta","delta":"hello"}"#)
        .expect("valid event");

    assert_eq!(event, Some(LlmEvent::TextDelta("hello".to_string())));
}

#[test]
fn parser_extracts_completed_response_id_and_usage() {
    let event = parse_openai_event(
        r#"{
          "type":"response.completed",
          "response":{
            "id":"resp_123",
            "usage":{
              "input_tokens":10,
              "output_tokens":4,
              "input_tokens_details":{"cached_tokens":3}
            }
          }
        }"#,
    )
    .expect("valid event");

    assert_eq!(
        event,
        Some(LlmEvent::Completed {
            response_id: Some("resp_123".to_string()),
            cost: CostSnapshot {
                input_tokens: Some(10),
                output_tokens: Some(4),
                cached_input_tokens: Some(3),
                estimated_usd_micros: None,
            },
        })
    );
}

#[test]
fn parser_surfaces_error_events() {
    let err = parse_openai_event(r#"{"type":"error","error":{"message":"bad request"}}"#)
        .expect_err("stream error");

    assert!(err.to_string().contains("bad request"));
}

#[test]
fn parser_surfaces_incomplete_events() {
    let err = parse_openai_event(
        r#"{
          "type":"response.incomplete",
          "response":{
            "incomplete_details":{"reason":"max_output_tokens"}
          }
        }"#,
    )
    .expect_err("incomplete response is a stream error");

    assert!(err.to_string().contains("max_output_tokens"));
}
