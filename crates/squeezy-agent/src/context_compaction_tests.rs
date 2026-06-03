use serde_json::json;
use squeezy_core::{AppConfig, ContextCompactionState};
use squeezy_llm::LlmInputItem;
use squeezy_tools::{ToolCostHint, ToolReceipt, ToolResult, ToolStatus, sha256_hex};

use super::{
    COMPACTION_DURABLE_LINES_LIMIT, COMPACTION_UNRESOLVED_LINES_LIMIT, PendingToolResult,
    build_compaction_summary, build_structured_compaction_prompt, durable_context_lines,
    estimate_context, is_structured_compaction_summary, pack_tool_results,
    strip_media_for_compaction, unresolved_question_lines,
};

fn function_call(call_id: &str, name: &str, arguments: serde_json::Value) -> LlmInputItem {
    LlmInputItem::FunctionCall {
        call_id: call_id.to_string(),
        name: name.to_string(),
        arguments,
    }
}

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
        content_parts: None,
        is_error: false,
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

fn lineage_block<'a>(summary: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>\n");
    let close = format!("\n</{tag}>");
    let start = summary.find(&open)? + open.len();
    let end_rel = summary[start..].find(&close)?;
    Some(&summary[start..start + end_rel])
}

#[test]
fn compaction_summary_emits_read_files_block() {
    // Two read_file calls land in <read-files>; the closing line of the
    // base summary stays put so the blocks really are an *append*.
    let older = vec![
        function_call(
            "call-1",
            "read_file",
            json!({"path": "crates/squeezy-tui/src/render/cache.rs"}),
        ),
        function_call(
            "call-2",
            "read_file",
            json!({"path": "crates/squeezy-llm/src/anthropic.rs"}),
        ),
    ];
    let state = ContextCompactionState::default();
    let config = AppConfig::default();

    let summary = build_compaction_summary(1, &state, &older, &[], None, &config);

    let body = lineage_block(&summary, "read-files").expect("<read-files> block missing");
    assert_eq!(
        body, "crates/squeezy-llm/src/anthropic.rs\ncrates/squeezy-tui/src/render/cache.rs",
        "read-files block content mismatch (alphabetic, deduped)"
    );
    assert!(
        !summary.contains("<modified-files>"),
        "modified block should not appear when no edits occurred"
    );
    assert!(
        summary.contains("Compacted 2 older model-visible item(s)"),
        "base summary tail must remain before the lineage blocks"
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
fn compaction_summary_emits_modified_files_block_for_write_apply_and_notebook() {
    // write_file, notebook_edit, and apply_patch all feed <modified-files>.
    // apply_patch is special: both legacy patches[] and modern operations[]
    // (including MoveFile's from/to) must populate the set.
    let older = vec![
        function_call(
            "call-1",
            "write_file",
            json!({"path": "crates/squeezy-tools/src/patch.rs", "content": "// ..."}),
        ),
        function_call(
            "call-2",
            "notebook_edit",
            json!({"path": "notebooks/explore.ipynb"}),
        ),
        function_call(
            "call-3",
            "apply_patch",
            json!({
                "patches": [
                    {"path": "crates/squeezy-agent/src/lib.rs", "search": "a", "replace": "b"}
                ],
                "operations": [
                    {"type": "move_file", "from": "old/file.rs", "to": "new/file.rs"},
                    {"type": "create_file", "path": "fresh/file.rs", "contents": ""}
                ]
            }),
        ),
    ];
    let state = ContextCompactionState::default();
    let config = AppConfig::default();

    let summary = build_compaction_summary(1, &state, &older, &[], None, &config);

    let body = lineage_block(&summary, "modified-files").expect("<modified-files> block missing");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(
        lines,
        vec![
            "crates/squeezy-agent/src/lib.rs",
            "crates/squeezy-tools/src/patch.rs",
            "fresh/file.rs",
            "new/file.rs",
            "notebooks/explore.ipynb",
            "old/file.rs",
        ],
        "modified-files block must include every write/apply_patch/notebook_edit path",
    );
    assert!(
        !summary.contains("<read-files>"),
        "read block should not appear when no reads occurred"
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
fn compaction_summary_modified_files_supersedes_read_files() {
    // Pi rule (computeFileLists): a file that is both read and modified
    // is reported only under <modified-files>.
    let older = vec![
        function_call("call-1", "read_file", json!({"path": "src/a.rs"})),
        function_call("call-2", "read_file", json!({"path": "src/b.rs"})),
        function_call(
            "call-3",
            "write_file",
            json!({"path": "src/a.rs", "content": "// ..."}),
        ),
    ];
    let state = ContextCompactionState::default();
    let config = AppConfig::default();

    let summary = build_compaction_summary(1, &state, &older, &[], None, &config);

    let read_body = lineage_block(&summary, "read-files").expect("<read-files> block missing");
    let modified_body =
        lineage_block(&summary, "modified-files").expect("<modified-files> block missing");
    assert_eq!(
        read_body, "src/b.rs",
        "src/a.rs should be promoted to modified-only",
    );
    assert_eq!(modified_body, "src/a.rs");
}

#[test]
fn compaction_summary_omits_lineage_blocks_when_no_file_ops() {
    // Search-class tools (grep) target a starting directory, not a file,
    // so they are intentionally excluded from the lineage map.
    let older = vec![
        LlmInputItem::UserText("hello".to_string()),
        function_call(
            "call-1",
            "grep",
            json!({"pattern": "todo", "path": "crates"}),
        ),
    ];
    let state = ContextCompactionState::default();
    let config = AppConfig::default();

    let summary = build_compaction_summary(1, &state, &older, &[], None, &config);

    assert!(
        !summary.contains("<read-files>"),
        "no file-class tools were invoked; <read-files> must be absent"
    );
    assert!(
        !summary.contains("<modified-files>"),
        "no file-class tools were invoked; <modified-files> must be absent"
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
fn compaction_summary_carries_lineage_across_generations() {
    // The prior summary already lists paths; the current `older` slice
    // adds new ones and promotes one read into modified. The output
    // must reflect the union, with modified-wins semantics and dedup.
    let previous = "Some prose.\n\
        <read-files>\n\
        prior/read-only.rs\n\
        prior/shared.rs\n\
        </read-files>\n\
        <modified-files>\n\
        prior/changed.rs\n\
        </modified-files>";
    let state = ContextCompactionState {
        summary: Some(previous.to_string()),
        ..ContextCompactionState::default()
    };

    let older = vec![
        function_call("call-1", "read_file", json!({"path": "current/look.rs"})),
        function_call(
            "call-2",
            "write_file",
            json!({"path": "prior/shared.rs", "content": "// ..."}),
        ),
    ];
    let config = AppConfig::default();

    let summary = build_compaction_summary(2, &state, &older, &[], None, &config);

    let read_body = lineage_block(&summary, "read-files").expect("<read-files> block missing");
    let modified_body =
        lineage_block(&summary, "modified-files").expect("<modified-files> block missing");
    assert_eq!(
        read_body, "current/look.rs\nprior/read-only.rs",
        "prior/shared.rs must be promoted out of read; prior/read-only.rs survives",
    );
    assert_eq!(
        modified_body, "prior/changed.rs\nprior/shared.rs",
        "modified set must accumulate across generations",
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
fn compaction_summary_caps_lineage_at_limit_keeping_newest() {
    // Build 60 read calls. The cap should fire and keep the 50 most
    // recent paths (i.e., drop the chronologically oldest 10). Sorted
    // output then makes the kept set easy to assert as `file_010..file_059`.
    let older: Vec<LlmInputItem> = (0..60)
        .map(|i| {
            function_call(
                &format!("call-{i}"),
                "read_file",
                json!({"path": format!("crates/a/file_{i:03}.rs")}),
            )
        })
        .collect();
    let state = ContextCompactionState::default();
    let config = AppConfig::default();

    let summary = build_compaction_summary(1, &state, &older, &[], None, &config);

    let body = lineage_block(&summary, "read-files").expect("<read-files> block missing");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(
        lines.len(),
        50,
        "lineage list must be capped at 50 entries; got {}",
        lines.len()
    );
    assert_eq!(
        lines.first(),
        Some(&"crates/a/file_010.rs"),
        "oldest-dropped: file_000..file_009 should have been evicted before sort",
    );
    assert_eq!(
        lines.last(),
        Some(&"crates/a/file_059.rs"),
        "newest entry must survive the cap",
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

#[test]
fn compaction_summary_dedups_repeated_file_touches() {
    // The same read_file call repeated 5 times still produces a single
    // entry in <read-files>.
    let older: Vec<LlmInputItem> = (0..5)
        .map(|i| {
            function_call(
                &format!("call-{i}"),
                "read_file",
                json!({"path": "crates/squeezy-core/src/lib.rs"}),
            )
        })
        .collect();
    let state = ContextCompactionState::default();
    let config = AppConfig::default();

    let summary = build_compaction_summary(1, &state, &older, &[], None, &config);

    let body = lineage_block(&summary, "read-files").expect("<read-files> block missing");
    assert_eq!(body, "crates/squeezy-core/src/lib.rs");
}

#[test]
fn strip_replaces_image_content_part_with_placeholder() {
    // A structured-result `Image` part carries raw bytes that the text
    // `output` scan never touches. Compaction must drop those bytes,
    // leaving a short placeholder, while preserving text parts (with their
    // inline data URIs scrubbed).
    let payload = long_base64_payload();
    let items = vec![LlmInputItem::FunctionCallOutput {
        call_id: "call-1".to_string(),
        output: "short".to_string(),
        content_parts: Some(vec![
            squeezy_llm::ToolResultPart::Text {
                text: format!("see data:image/png;base64,{payload} done"),
            },
            squeezy_llm::ToolResultPart::Image {
                media_type: "image/png".to_string(),
                bytes: vec![0u8; 4096].into(),
            },
        ]),
        is_error: false,
    }];

    let stripped = strip_media_for_compaction(&items);
    let LlmInputItem::FunctionCallOutput { content_parts, .. } = &stripped[0] else {
        panic!("expected FunctionCallOutput");
    };
    let parts = content_parts.as_ref().expect("content_parts retained");
    assert_eq!(parts.len(), 2, "part count changed; got {parts:?}");

    match &parts[0] {
        squeezy_llm::ToolResultPart::Text { text } => {
            assert!(
                text.contains("[image]"),
                "data URI not scrubbed; got {text:?}"
            );
            assert!(
                !text.contains(payload.as_str()),
                "base64 payload leaked through text part; got {text:?}"
            );
        }
        other => panic!("expected text part, got {other:?}"),
    }
    match &parts[1] {
        squeezy_llm::ToolResultPart::Text { text } => {
            assert_eq!(text, "[image]", "image part not replaced; got {text:?}");
        }
        squeezy_llm::ToolResultPart::Image { .. } => {
            panic!("image bytes survived compaction")
        }
    }
}

#[test]
fn strip_rebuilds_parts_even_when_output_is_short() {
    // Output below STRIP_MEDIA_MIN_LEN would normally clone through, but a
    // populated `content_parts` must still be shrunk; the short `output`
    // string itself is preserved verbatim.
    let items = vec![LlmInputItem::FunctionCallOutput {
        call_id: "call-1".to_string(),
        output: "tiny".to_string(),
        content_parts: Some(vec![squeezy_llm::ToolResultPart::Image {
            media_type: "image/png".to_string(),
            bytes: vec![1u8; 2048].into(),
        }]),
        is_error: false,
    }];

    let stripped = strip_media_for_compaction(&items);
    let LlmInputItem::FunctionCallOutput {
        output,
        content_parts,
        ..
    } = &stripped[0]
    else {
        panic!("expected FunctionCallOutput");
    };
    assert_eq!(output, "tiny", "short output should pass through unchanged");
    let parts = content_parts.as_ref().expect("content_parts retained");
    assert!(
        matches!(&parts[0], squeezy_llm::ToolResultPart::Text { text } if text == "[image]"),
        "image part not stripped; got {parts:?}"
    );
}

#[test]
fn estimate_context_counts_content_parts_bytes() {
    // Image bytes living only in `content_parts` must register as context
    // pressure; counting `output.len()` alone would render a multi-KB
    // screenshot invisible to compaction.
    let image_bytes = 8192usize;
    let with_parts = vec![LlmInputItem::FunctionCallOutput {
        call_id: "call-1".to_string(),
        output: "ok".to_string(),
        content_parts: Some(vec![squeezy_llm::ToolResultPart::Image {
            media_type: "image/png".to_string(),
            bytes: vec![0u8; image_bytes].into(),
        }]),
        is_error: false,
    }];
    let without_parts = vec![LlmInputItem::FunctionCallOutput {
        call_id: "call-1".to_string(),
        output: "ok".to_string(),
        content_parts: None,
        is_error: false,
    }];

    let with = estimate_context(&with_parts);
    let without = estimate_context(&without_parts);
    assert!(
        with.bytes >= without.bytes + image_bytes,
        "content_parts bytes not billed: with={} without={}",
        with.bytes,
        without.bytes
    );
}

#[test]
fn durable_context_lines_keep_most_recent_when_capped() {
    // Build more durable items than the cap, each uniquely numbered so the
    // retained window is unambiguous. The slice is chronological (oldest
    // first), so the cap must keep the LAST N, not the first N.
    let total = COMPACTION_DURABLE_LINES_LIMIT + 5;
    let items: Vec<LlmInputItem> = (0..total)
        .map(|i| LlmInputItem::UserText(format!("fact {i}")))
        .collect();

    let lines = durable_context_lines(&items);
    assert_eq!(lines.len(), COMPACTION_DURABLE_LINES_LIMIT);
    assert_eq!(
        lines.first().unwrap(),
        &format!("- user: fact {}", total - COMPACTION_DURABLE_LINES_LIMIT),
        "oldest retained line should be the first of the most-recent window"
    );
    assert_eq!(
        lines.last().unwrap(),
        &format!("- user: fact {}", total - 1),
        "most recent durable item must survive compaction"
    );
}

#[test]
fn unresolved_question_lines_keep_most_recent_when_capped() {
    let total = COMPACTION_UNRESOLVED_LINES_LIMIT + 4;
    let items: Vec<LlmInputItem> = (0..total)
        .map(|i| LlmInputItem::UserText(format!("question {i}?")))
        .collect();

    let lines = unresolved_question_lines(&items);
    assert_eq!(lines.len(), COMPACTION_UNRESOLVED_LINES_LIMIT);
    assert_eq!(
        lines.first().unwrap(),
        &format!("- question {}?", total - COMPACTION_UNRESOLVED_LINES_LIMIT),
        "oldest retained question should be the first of the most-recent window"
    );
    assert_eq!(
        lines.last().unwrap(),
        &format!("- question {}?", total - 1),
        "most recent open question must survive compaction"
    );
}

fn tool_result(
    call_id: &str,
    tool_name: &str,
    status: ToolStatus,
    content: serde_json::Value,
) -> ToolResult {
    let output_bytes = serde_json::to_vec(&content).unwrap();
    ToolResult {
        call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        status,
        content,
        cost_hint: ToolCostHint::default(),
        receipt: ToolReceipt {
            output_sha256: sha256_hex(&output_bytes),
            content_sha256: None,
        },
        spill_model_output: None,
    }
}

fn omitted_to_stub(result: &ToolResult) -> bool {
    // `aggregate_budget_exceeded` rewrites the omitted result to an Error
    // carrying `original_output_sha256` (the sha-bearing stub).
    result.status == ToolStatus::Error
        && result
            .content
            .get("original_output_sha256")
            .and_then(serde_json::Value::as_str)
            .is_some()
}

#[test]
fn pack_tool_results_prioritizes_small_error_over_large_read_under_tight_budget() {
    // Input order is [large read, small error], so input-order packing would
    // spend the whole budget on the large read and omit the error. Priority
    // packing must reverse this: the small error is retained, the large read
    // is the one pushed past the budget and degraded to a sha-bearing stub.
    let large_read = tool_result(
        "call-read",
        "read_file",
        ToolStatus::Success,
        json!({ "path": "src/big.rs", "content": "x".repeat(4096) }),
    );
    let small_error = tool_result(
        "call-grep",
        "grep",
        ToolStatus::Error,
        json!({ "error": "regex compile failed: unbalanced parenthesis" }),
    );

    let large_bytes = large_read.model_output().len();
    let small_bytes = small_error.model_output().len();
    // Budget admits the small error but not both — so exactly one survives.
    let budget = small_bytes + (large_bytes - small_bytes) / 2;
    assert!(
        budget >= small_bytes && budget < large_bytes,
        "test budget must fit the small error but not the large read"
    );

    let packed = pack_tool_results(
        vec![
            PendingToolResult::plain(large_read.clone()),
            PendingToolResult::plain(small_error.clone()),
        ],
        budget,
    );

    // call-ids and their positions are preserved (only inclusion changes).
    assert_eq!(packed.len(), 2);
    assert_eq!(packed[0].result.call_id, "call-read");
    assert_eq!(packed[1].result.call_id, "call-grep");

    let read_out = &packed[0].result;
    let error_out = &packed[1].result;

    // The small error survives intact; the large read is omitted-to-stub.
    assert_eq!(
        error_out.status,
        ToolStatus::Error,
        "small tool error must be retained under budget pressure"
    );
    assert!(
        !omitted_to_stub(error_out),
        "small error must NOT be omitted to a budget stub"
    );
    assert!(
        error_out.model_output().contains("unbalanced parenthesis"),
        "retained error must keep its original content, got {:?}",
        error_out.content
    );
    assert!(
        omitted_to_stub(read_out),
        "large read must be omitted to a sha-bearing budget stub, got {:?}",
        read_out.content
    );
    // The stub stays recoverable: it carries the original output sha.
    assert_eq!(
        read_out
            .content
            .get("original_output_sha256")
            .and_then(serde_json::Value::as_str),
        Some(large_read.receipt.output_sha256.as_str()),
    );
}
