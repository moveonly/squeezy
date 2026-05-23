use super::*;

#[test]
fn app_starts_ready_with_empty_transcript() {
    let app = TuiApp::new("openai", "gpt-test".to_string());

    assert_eq!(app.provider_name, "openai");
    assert_eq!(app.model, "gpt-test");
    assert_eq!(app.status, "ready");
    assert!(app.transcript.is_empty());
}

#[test]
fn transcript_item_formats_role_label() {
    let item = TranscriptItem::user("hello");
    let line = format_transcript_item(&item);
    let text = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();

    assert_eq!(text, "user hello");
}
