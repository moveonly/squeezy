use squeezy_core::{AppConfig, ContextCompactionState};
use squeezy_llm::LlmInputItem;

use super::{
    build_compaction_summary, build_structured_compaction_prompt, is_structured_compaction_summary,
    strip_media_for_compaction,
};

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

#[test]
fn structured_compaction_prompt_pins_all_four_slot_names() {
    // The whole point of the structured template is that the model-assisted
    // prompt names exactly the four slots that survive across N compactions.
    // If any of these strings drift, the slot validator and the file-lineage
    // sibling pass (which appends `<read-files>` / `<modified-files>` below
    // `## Next`) lose their shared contract.
    let prompt = build_structured_compaction_prompt(None, "extractive body", 500);
    for slot in ["## Goal", "## Progress", "## Decisions", "## Next"] {
        assert!(
            prompt.contains(slot),
            "structured compaction prompt is missing slot header {slot}; prompt was:\n{prompt}"
        );
    }
    assert!(
        prompt.contains("<new-conversation>") && prompt.contains("</new-conversation>"),
        "prompt must wrap the new extractive output in a `<new-conversation>` block"
    );
    assert!(
        prompt.contains("Do NOT invent new facts")
            || prompt.contains("Do NOT omit prior decisions"),
        "prompt must carry the no-fabrication / no-decision-drop guardrails: {prompt}"
    );
    assert!(
        prompt.contains("500"),
        "prompt must surface the configured max_output_tokens budget"
    );
}

#[test]
fn structured_compaction_prompt_attaches_previous_summary_block_when_present() {
    // Iterative update is the load-bearing piece of F12-pi-iterative-summary-update.
    // The model must see the prior compaction's output as a *separate* tagged
    // block, not just inline inside the new extractive body, so it can carry
    // forward `## Decisions` and `## Progress` entries deterministically.
    // Check for the actual block opening `<previous-summary>\n` rather than
    // the bare tag string, which the Rules text also references (e.g.
    // "PRESERVE every entry from `<previous-summary>` ...").
    let prev = "## Goal\nbuild a parser\n\n## Decisions\n- use tree-sitter";
    let prompt = build_structured_compaction_prompt(Some(prev), "extractive body", 800);

    assert!(
        prompt.contains("<previous-summary>\n") && prompt.contains("\n</previous-summary>\n"),
        "prompt must wrap the prior summary in a `<previous-summary>` block when one exists"
    );
    assert!(
        prompt.contains("use tree-sitter"),
        "prompt must embed the verbatim prior summary contents"
    );
    assert!(
        prompt.contains("PRESERVE every entry from `<previous-summary>`"),
        "prompt must instruct the model to preserve prior slot entries"
    );
}

#[test]
fn structured_compaction_prompt_omits_previous_summary_block_on_cold_start() {
    // First-ever compaction has no prior summary. The actual
    // `<previous-summary>\n` block opening should be absent — emitting an
    // empty block would tempt the model to fabricate "prior decisions"
    // from thin air, and the iterative-update contract explicitly forbids
    // that. The Rules text still mentions the block by name so the model
    // knows the slot semantics; assert only on the block opening, not the
    // bare tag string.
    let prompt = build_structured_compaction_prompt(None, "extractive body", 500);
    assert!(
        !prompt.contains("<previous-summary>\n"),
        "cold-start prompt must not emit a `<previous-summary>` block opening"
    );

    // Whitespace-only previous summary is treated the same as `None` —
    // it carries no slot content worth preserving.
    let prompt_blank =
        build_structured_compaction_prompt(Some("   \n\n  "), "extractive body", 500);
    assert!(
        !prompt_blank.contains("<previous-summary>\n"),
        "blank previous summary must not produce a `<previous-summary>` block opening"
    );
}

#[test]
fn is_structured_compaction_summary_accepts_complete_template() {
    let body = "\
## Goal\nbuild a parser\n\n\
## Progress\n- wrote lexer\n\n\
## Decisions\n- use tree-sitter\n\n\
## Next\n- wire grammar tests\n";
    assert!(
        is_structured_compaction_summary(body),
        "complete four-slot output should validate"
    );
}

#[test]
fn is_structured_compaction_summary_accepts_lenient_heading_variants() {
    // Models drift in predictable ways: deeper heading levels, trailing
    // colons, and decorator words like "Key Decisions" or "Next Steps" all
    // still represent the four slots and must validate. The validator only
    // catches *missing* slots, not stylistic variation.
    let body = "\
### Goal:\nship structured compaction\n\n\
## Progress\n- merged prompt change\n\n\
## Key Decisions\n- match keyword as whole word\n\n\
## Next Steps\n- ship file-lineage sibling\n";
    assert!(
        is_structured_compaction_summary(body),
        "lenient header variants should validate; body was:\n{body}"
    );
}

#[test]
fn is_structured_compaction_summary_accepts_file_lineage_blocks_below_next() {
    // The file-lineage sibling pass (F12-pi-file-lineage-in-summary) appends
    // `<read-files>` / `<modified-files>` XML blocks below `## Next`. The
    // validator must not reject the document just because more content
    // appears after the fourth slot.
    let body = "\
## Goal\nbuild a parser\n\n\
## Progress\n- wrote lexer\n\n\
## Decisions\n- use tree-sitter\n\n\
## Next\n- wire grammar tests\n\n\
<read-files>\n/repo/src/parser.rs\n</read-files>\n\n\
<modified-files>\n/repo/src/lexer.rs\n</modified-files>\n";
    assert!(
        is_structured_compaction_summary(body),
        "file-lineage trailer must not invalidate the structured output"
    );
}

#[test]
fn is_structured_compaction_summary_rejects_missing_slot() {
    // Drop `## Decisions` — the slot most likely to be silently lost under
    // the old "rewrite verbatim" prompt. The validator must reject so the
    // caller can fall back to the deterministic extractive baseline.
    let body = "\
## Goal\nbuild a parser\n\n\
## Progress\n- wrote lexer\n\n\
## Next\n- wire grammar tests\n";
    assert!(
        !is_structured_compaction_summary(body),
        "missing `## Decisions` slot must invalidate the structured output"
    );
}

#[test]
fn is_structured_compaction_summary_rejects_free_text_output() {
    // Legacy "rewrite verbatim" output is plain prose with no markdown
    // headings. The validator should reject so the strategy gate falls
    // back to the extractive summary instead of accepting an unstructured
    // blob the file-lineage append pass cannot anchor onto.
    let body = "We rewrote the conversation summary. Decisions were preserved. \
                Next step is to wire grammar tests.";
    assert!(
        !is_structured_compaction_summary(body),
        "free-text output without headings must fail validation"
    );
}

#[test]
fn is_structured_compaction_summary_rejects_keyword_in_prose_without_heading() {
    // A model that prepends commentary like "Goal: foo\nProgress: bar" using
    // plain text labels rather than markdown headings must not pass — the
    // file-lineage / TUI render pipeline both rely on the `##` heading shape
    // to split the document into slots.
    let body = "Goal: build a parser\n\
                Progress: wrote lexer\n\
                Decisions: use tree-sitter\n\
                Next: wire grammar tests\n";
    assert!(
        !is_structured_compaction_summary(body),
        "plain-text labels without `#` headings must fail validation"
    );
}
