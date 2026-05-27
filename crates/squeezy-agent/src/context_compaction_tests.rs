use squeezy_core::{AppConfig, ContextCompactionState};
use squeezy_llm::LlmInputItem;

use super::{build_compaction_summary, strip_media_for_compaction};

/// A 220-byte base64 blob built from a repeating pattern. Long enough to
/// exceed `STRIP_MEDIA_MIN_LEN` (100) and to survive `compact_text`'s
/// 260-char tool-output cap, so a leaked URI would land in the summary
/// without this guard.
fn long_base64_payload() -> String {
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".repeat(4)
}

fn function_call_output(call_id: &str, output: &str) -> LlmInputItem {
    LlmInputItem::FunctionCallOutput {
        call_id: call_id.to_string(),
        output: output.to_string(),
    }
}

#[test]
fn strip_image_data_uri_from_function_call_output() {
    let payload = long_base64_payload();
    let body = format!("Screenshot saved. data:image/png;base64,{payload} (end of output)");
    let items = vec![function_call_output("call-1", &body)];

    let stripped = strip_media_for_compaction(&items);
    let LlmInputItem::FunctionCallOutput { output, .. } = &stripped[0] else {
        panic!("expected FunctionCallOutput");
    };

    assert!(
        output.contains("[image]"),
        "placeholder missing; got {output:?}"
    );
    assert!(
        !output.contains("data:image/png;base64,"),
        "data URI prefix leaked through; got {output:?}"
    );
    assert!(
        !output.contains(payload.as_str()),
        "base64 payload leaked through; got {output:?}"
    );
    assert!(
        output.starts_with("Screenshot saved."),
        "leading prose dropped; got {output:?}"
    );
    assert!(
        output.ends_with("(end of output)"),
        "trailing prose dropped; got {output:?}"
    );
}

#[test]
fn strip_document_data_uri_uses_document_placeholder() {
    let payload = long_base64_payload();
    let body = format!("report attached: data:application/pdf;base64,{payload}");
    let items = vec![function_call_output("call-1", &body)];

    let stripped = strip_media_for_compaction(&items);
    let LlmInputItem::FunctionCallOutput { output, .. } = &stripped[0] else {
        panic!("expected FunctionCallOutput");
    };

    assert!(
        output.contains("[document]"),
        "document placeholder missing; got {output:?}"
    );
    assert!(
        !output.contains("base64,"),
        "data URI marker leaked; got {output:?}"
    );
}

#[test]
fn strip_handles_multiple_uris_in_one_output() {
    let payload = long_base64_payload();
    let body = format!(
        "first data:image/jpeg;base64,{payload} between data:image/webp;base64,{payload} tail"
    );
    let items = vec![function_call_output("call-1", &body)];

    let stripped = strip_media_for_compaction(&items);
    let LlmInputItem::FunctionCallOutput { output, .. } = &stripped[0] else {
        panic!("expected FunctionCallOutput");
    };

    assert_eq!(
        output.matches("[image]").count(),
        2,
        "expected two placeholders; got {output:?}"
    );
    assert!(output.starts_with("first "));
    assert!(output.contains(" between "));
    assert!(output.ends_with(" tail"));
}

#[test]
fn strip_media_does_not_touch_in_memory_state() {
    let payload = long_base64_payload();
    let body = format!("data:image/png;base64,{payload}");
    let original = vec![
        LlmInputItem::UserText("hello".to_string()),
        function_call_output("call-1", &body),
    ];
    let snapshot = original.clone();

    let _ = strip_media_for_compaction(&original);

    assert_eq!(original, snapshot, "input slice was mutated");
}

#[test]
fn strip_leaves_non_function_call_output_items_unchanged() {
    let payload = long_base64_payload();
    let body = format!("data:image/png;base64,{payload}");
    // A UserText with a data URI is left alone: the recommendation
    // targets FunctionCallOutput because that is the realistic ingress
    // path for tool-produced screenshots/PDFs. User prose with an inline
    // data URI is a knowing decision by the user.
    let items = vec![LlmInputItem::UserText(body.clone())];
    let stripped = strip_media_for_compaction(&items);
    assert_eq!(stripped, items);
}

#[test]
fn strip_skips_short_outputs() {
    // Anything under STRIP_MEDIA_MIN_LEN (100) is cloned through unchanged
    // so plain short tool outputs don't pay the scan cost.
    let body = "short output, no media";
    let items = vec![function_call_output("call-1", body)];
    let stripped = strip_media_for_compaction(&items);
    let LlmInputItem::FunctionCallOutput { output, .. } = &stripped[0] else {
        panic!("expected FunctionCallOutput");
    };
    assert_eq!(output, body);
}

#[test]
fn strip_preserves_unicode_neighbours() {
    let payload = long_base64_payload();
    // Multi-byte UTF-8 on both sides of the data URI. Byte-index handling
    // would corrupt these scalars if the strip scanner ever sliced inside
    // a code point.
    let body = format!("héllo data:image/png;base64,{payload} 世界");
    let items = vec![function_call_output("call-1", &body)];
    let stripped = strip_media_for_compaction(&items);
    let LlmInputItem::FunctionCallOutput { output, .. } = &stripped[0] else {
        panic!("expected FunctionCallOutput");
    };
    assert!(output.contains("héllo "));
    assert!(output.contains(" 世界"));
    assert!(output.contains("[image]"));
}

#[test]
fn compaction_summary_does_not_carry_base64_image_payload() {
    // build_compaction_summary is invoked on the stripped older slice in
    // compact_conversation (see context_compaction.rs:148-167). If the
    // tool output contained a base64 PNG, the model-assisted summarizer
    // would otherwise receive it via `extractive_summary`. Verify the
    // built summary does not contain the raw base64 string.
    let payload = long_base64_payload();
    let body = format!("screenshot ready. data:image/png;base64,{payload} ok.");
    let older = vec![
        LlmInputItem::UserText("write a screenshot".to_string()),
        function_call_output("call-1", &body),
    ];
    let older_for_summary = strip_media_for_compaction(&older);

    let state = ContextCompactionState::default();
    let config = AppConfig::default();
    let summary = build_compaction_summary(1, &state, &older_for_summary, &[], None, &config);

    assert!(
        !summary.contains(payload.as_str()),
        "base64 payload reached the compaction summary"
    );
    assert!(
        !summary.contains("data:image/png;base64,"),
        "data URI prefix reached the compaction summary"
    );
}
