use super::*;
use serde_json::json;
use squeezy_core::{AppConfig, ContextCompactionConfig};
use squeezy_llm::LlmInputItem;

fn read_file_pair(n: usize, body_len: usize) -> Vec<LlmInputItem> {
    let call_id = format!("call_{n:03}");
    let body = "x".repeat(body_len);
    vec![
        LlmInputItem::FunctionCall {
            call_id: call_id.clone(),
            name: "read_file".to_string(),
            arguments: json!({ "path": format!("file_{n}.rs") }),
        },
        LlmInputItem::FunctionCallOutput {
            call_id,
            output: body,
            content_parts: None,
            is_error: false,
        },
    ]
}

fn config_with_micro(window: u64, threshold: u8, keep_recent: usize) -> AppConfig {
    AppConfig {
        context_compaction: ContextCompactionConfig {
            enabled_mid_turn: true,
            model_context_window: Some(window),
            // Neutralize the effective-window reduction so `trim_threshold` is a
            // clean percent of the small test window (the 12K baseline reserve
            // would otherwise swallow these sub-window budgets).
            effective_context_window_percent: Some(100),
            baseline_reserve_tokens: Some(0),
            micro_compaction_enabled: true,
            trim_at_percent: threshold,
            micro_compaction_keep_recent: keep_recent,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    }
}

#[test]
fn micro_compact_preserves_recent_n() {
    let config = config_with_micro(10_000, 50, 5);
    let mut conversation = Vec::new();
    for n in 0..12 {
        conversation.extend(read_file_pair(n, 4_000));
    }
    let report = maybe_micro_compact(&mut conversation, &config, Some(9_000))
        .expect("micro-compaction should fire at saturation");
    assert_eq!(
        report.cleared_call_ids.len(),
        7,
        "expected 12 outputs minus 5 keep_recent to be cleared, got {}",
        report.cleared_call_ids.len(),
    );
    let outputs: Vec<&str> = conversation
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::FunctionCallOutput { output, .. } => Some(output.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(outputs.len(), 12);
    for output in &outputs[..7] {
        assert!(
            output.starts_with(MICRO_COMPACT_CLEARED_PREFIX),
            "older output should be cleared: {output}",
        );
    }
    for output in &outputs[7..] {
        assert!(
            !output.starts_with(MICRO_COMPACT_CLEARED_PREFIX),
            "recent output should be verbatim, got cleared marker: {output}",
        );
        assert_eq!(output.len(), 4_000);
    }
}

#[test]
fn micro_compact_preserves_message_structure() {
    let config = config_with_micro(10_000, 50, 5);
    let mut conversation = Vec::new();
    conversation.push(LlmInputItem::UserText("kickoff prompt".to_string()));
    for n in 0..12 {
        conversation.extend(read_file_pair(n, 4_000));
    }
    conversation.push(LlmInputItem::AssistantText("thinking".to_string()));
    let original_len = conversation.len();
    let _ = maybe_micro_compact(&mut conversation, &config, Some(9_000))
        .expect("micro-compaction should fire");
    assert_eq!(
        conversation.len(),
        original_len,
        "micro-compaction must not change the conversation length",
    );
    let mut calls = std::collections::BTreeSet::new();
    let mut outputs = std::collections::BTreeSet::new();
    for item in &conversation {
        match item {
            LlmInputItem::FunctionCall { call_id, .. } => {
                calls.insert(call_id.clone());
            }
            LlmInputItem::FunctionCallOutput { call_id, .. } => {
                outputs.insert(call_id.clone());
            }
            _ => {}
        }
    }
    assert_eq!(
        calls, outputs,
        "every function call must keep its output pair",
    );
}

#[test]
fn micro_compact_then_full_compact_works() {
    // After a micro pass clears the heavy bodies, the full-tier compaction
    // path can still run on the resulting conversation and produce its
    // summary head plus the recent slice.
    let mut config = config_with_micro(10_000, 50, 5);
    config.context_compaction.recent_items = 4;
    config.context_compaction.min_items = 1;
    let mut conversation = Vec::new();
    for n in 0..12 {
        conversation.extend(read_file_pair(n, 4_000));
    }
    let micro =
        maybe_micro_compact(&mut conversation, &config, Some(9_000)).expect("micro should fire");
    assert!(micro.bytes_saved > 0);

    let mut state = squeezy_core::ContextCompactionState::default();
    let report = crate::context_compaction::compact_conversation(
        &mut conversation,
        &mut state,
        &[],
        None,
        &config,
        squeezy_core::ContextCompactionTrigger::Auto,
        true,
        0,
    )
    .expect("full compaction should fire");
    assert!(matches!(
        conversation.first(),
        Some(LlmInputItem::UserText(_)),
    ));
    assert!(
        report.record.dropped_items > 0,
        "full compaction should report dropped items",
    );
    assert!(conversation.len() <= 5);
}

#[test]
fn micro_compact_skips_when_disabled() {
    let mut config = config_with_micro(10_000, 50, 5);
    config.context_compaction.micro_compaction_enabled = false;
    let mut conversation = Vec::new();
    for n in 0..12 {
        conversation.extend(read_file_pair(n, 4_000));
    }
    let report = maybe_micro_compact(&mut conversation, &config, Some(9_000));
    assert!(report.is_none());
}

#[test]
fn micro_compact_skips_below_threshold() {
    let config = config_with_micro(10_000, 50, 5);
    let mut conversation = Vec::new();
    for n in 0..12 {
        conversation.extend(read_file_pair(n, 4_000));
    }
    let report = maybe_micro_compact(&mut conversation, &config, Some(1_000));
    assert!(report.is_none());
}

#[test]
fn micro_compact_skips_non_compactable_tools() {
    let config = config_with_micro(10_000, 50, 5);
    let mut conversation = Vec::new();
    for n in 0..12 {
        let call_id = format!("call_{n:03}");
        let body = "x".repeat(4_000);
        conversation.push(LlmInputItem::FunctionCall {
            call_id: call_id.clone(),
            name: "notes_recall".to_string(),
            arguments: json!({}),
        });
        conversation.push(LlmInputItem::FunctionCallOutput {
            call_id,
            output: body,
            content_parts: None,
            is_error: false,
        });
    }
    let report = maybe_micro_compact(&mut conversation, &config, Some(9_000));
    assert!(
        report.is_none(),
        "notes_recall is not in the compactable set; nothing should clear",
    );
}

#[test]
fn micro_compact_is_idempotent_on_already_cleared_outputs() {
    let config = config_with_micro(10_000, 50, 5);
    let mut conversation = Vec::new();
    for n in 0..12 {
        conversation.extend(read_file_pair(n, 4_000));
    }
    let _first = maybe_micro_compact(&mut conversation, &config, Some(9_000))
        .expect("first pass should fire");
    let second = maybe_micro_compact(&mut conversation, &config, Some(9_000));
    assert!(
        second.is_none(),
        "second micro pass should be a no-op when older outputs already carry the placeholder",
    );
}

// --- M2: expired-context masking by file-mutation lineage ---------------

/// A `read_file` call+output pair where the output body is the literal
/// `content` (callers pass the model-visible body string directly so the
/// substring match exercises real text, not JSON wrapping).
fn read_pair(call_id: &str, path: &str, content: &str) -> Vec<LlmInputItem> {
    vec![
        LlmInputItem::FunctionCall {
            call_id: call_id.to_string(),
            name: "read_file".to_string(),
            arguments: json!({ "path": path }),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: content.to_string(),
            content_parts: None,
            is_error: false,
        },
    ]
}

fn output_for(conversation: &[LlmInputItem], call_id: &str) -> String {
    conversation
        .iter()
        .find_map(|item| match item {
            LlmInputItem::FunctionCallOutput {
                call_id: id,
                output,
                ..
            } if id == call_id => Some(output.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no output for {call_id}"))
}

#[test]
fn mask_expired_splices_only_the_changed_span() {
    // A read of foo.rs captured three lines. A later edit replaces the
    // middle line's body. The prior read's changed span must be stubbed,
    // the surrounding lines preserved byte-for-byte. The span is larger
    // than the recovery stub so masking is a net byte win.
    let before_span =
        "    let total = old_compute(items, options, &mut accumulator, fallback_default);";
    let body = format!("fn run() {{\n{before_span}\n    println!(\"{{total}}\");\n}}");
    let mut conversation = read_pair("r1", "foo.rs", &body);

    let edits = vec![SuccessfulEdit {
        path: "foo.rs".to_string(),
        changed_spans: vec![before_span.to_string()],
        whole_file: false,
    }];
    let report = mask_expired_reads_after_edits(&mut conversation, &edits, 0)
        .expect("a successful edit to foo.rs must mask the stale read span");
    assert_eq!(report.spans_masked, 1);
    assert_eq!(report.masked_call_ids, vec!["r1".to_string()]);
    assert!(report.bytes_saved > 0);

    let masked = output_for(&conversation, "r1");
    assert!(
        !masked.contains(before_span),
        "the pre-edit span must be gone: {masked}",
    );
    assert!(
        masked.contains(MICRO_COMPACT_CLEARED_PREFIX),
        "a recovery stub must replace the span: {masked}",
    );
    // Content outside the changed range survives verbatim.
    assert!(
        masked.starts_with("fn run() {\n"),
        "leading context must survive: {masked}",
    );
    assert!(
        masked.contains("    println!(\"{total}\");\n}"),
        "trailing context must survive: {masked}",
    );
}

#[test]
fn mask_expired_leaves_unrelated_reads_untouched() {
    // foo.rs is edited; a read of bar.rs that happens to share no changed
    // span must stay verbatim.
    let foo_span =
        "let x = stale_value(config, registry, &mut buffer, retry_policy_default, extra_arg_here);";
    let foo_body = format!("fn a() {{ {foo_span} }}");
    let bar_body = "fn b() { let y = independent(); }".to_string();
    let mut conversation = Vec::new();
    conversation.extend(read_pair("foo_read", "foo.rs", &foo_body));
    conversation.extend(read_pair("bar_read", "bar.rs", &bar_body));

    let edits = vec![SuccessfulEdit {
        path: "foo.rs".to_string(),
        changed_spans: vec![foo_span.to_string()],
        whole_file: false,
    }];
    let report = mask_expired_reads_after_edits(&mut conversation, &edits, 0)
        .expect("foo.rs read should be masked");
    assert_eq!(report.masked_call_ids, vec!["foo_read".to_string()]);

    assert_eq!(
        output_for(&conversation, "bar_read"),
        bar_body,
        "an unrelated read of bar.rs must not be touched",
    );
}

#[test]
fn mask_expired_keeps_the_freshest_read_that_informed_the_edit() {
    // Two reads of foo.rs both contain the changed span. With
    // keep_recent=1 only the older snapshot is masked; the freshest read
    // (the one that plausibly informed the edit) stays verbatim.
    let span =
        "fn target() { legacy_body(input, ctx, &mut sink, deprecated_flag_default, extra_param); }";
    let body = format!("// header\n{span}\n// footer");
    let mut conversation = Vec::new();
    conversation.extend(read_pair("old_read", "foo.rs", &body));
    conversation.extend(read_pair("fresh_read", "foo.rs", &body));

    let edits = vec![SuccessfulEdit {
        path: "foo.rs".to_string(),
        changed_spans: vec![span.to_string()],
        whole_file: false,
    }];
    let report = mask_expired_reads_after_edits(&mut conversation, &edits, 1)
        .expect("the older snapshot should be masked");
    assert_eq!(report.masked_call_ids, vec!["old_read".to_string()]);

    assert!(
        !output_for(&conversation, "old_read").contains(span),
        "the stale older read must lose the span",
    );
    assert_eq!(
        output_for(&conversation, "fresh_read"),
        body,
        "the freshest read that informed the edit must stay verbatim",
    );
}

#[test]
fn mask_expired_does_nothing_for_errored_or_denied_edits() {
    // The caller (collect_successful_edits) only forwards Success edits,
    // so an errored/denied edit produces an empty `edits` slice and this
    // pass is a strict no-op — the prior read stays authoritative.
    let span = "old = compute();";
    let body = format!("fn a() {{ {span} }}");
    let mut conversation = read_pair("r1", "foo.rs", &body);
    let original = conversation.clone();

    // Empty edits = what an errored/denied/cancelled edit yields upstream.
    let report = mask_expired_reads_after_edits(&mut conversation, &[], 0);
    assert!(report.is_none(), "no successful edit => no masking");
    assert_eq!(conversation, original, "the read must be untouched");
}

#[test]
fn mask_expired_handles_json_escaped_multiline_spans() {
    // Real read outputs are JSON tool-result bodies, so a multi-line
    // search span appears escaped (\n, \") in the snapshot. The pass
    // escapes the span the same way before matching.
    let span = "let s = \"alpha_beta_gamma\";\nlet t = old_legacy_compute(config, registry);";
    let escaped = serde_json::Value::String(span.to_string()).to_string();
    let inner = escaped.trim_matches('"');
    let body = format!("{{\"content\":\"prefix {inner} suffix\"}}");
    let mut conversation = read_pair("r1", "foo.rs", &body);

    let edits = vec![SuccessfulEdit {
        path: "foo.rs".to_string(),
        changed_spans: vec![span.to_string()],
        whole_file: false,
    }];
    let report = mask_expired_reads_after_edits(&mut conversation, &edits, 0)
        .expect("the escaped multi-line span must match and mask");
    assert_eq!(report.spans_masked, 1);
    let masked = output_for(&conversation, "r1");
    assert!(
        !masked.contains(inner),
        "the escaped span must be replaced: {masked}",
    );
    assert!(masked.contains("prefix "), "surrounding text survives");
    assert!(masked.contains(" suffix"), "surrounding text survives");
}

#[test]
fn mask_expired_whole_file_overwrite_masks_entire_prior_read() {
    // write_file is a full-file overwrite with no sub-span; every prior
    // snapshot of the path is stale and masked whole, with a recovery
    // stub. keep_recent still protects the freshest snapshot.
    let body = "fn a() {}\nfn b() {}\n".to_string();
    let mut conversation = Vec::new();
    conversation.extend(read_pair("old_read", "foo.rs", &body));
    conversation.extend(read_pair("fresh_read", "foo.rs", &body));

    let edits = vec![SuccessfulEdit {
        path: "foo.rs".to_string(),
        changed_spans: Vec::new(),
        whole_file: true,
    }];
    let report = mask_expired_reads_after_edits(&mut conversation, &edits, 1)
        .expect("a write_file overwrite masks the older snapshot");
    assert_eq!(report.masked_call_ids, vec!["old_read".to_string()]);
    assert!(
        output_for(&conversation, "old_read").starts_with(MICRO_COMPACT_CLEARED_PREFIX),
        "older whole-file snapshot must be stubbed",
    );
    assert_eq!(
        output_for(&conversation, "fresh_read"),
        body,
        "freshest snapshot stays verbatim under keep_recent",
    );
}

#[test]
fn mask_expired_skips_repo_wide_grep_without_path() {
    // A grep with no `path` scope has no single edited file to attribute,
    // so it is never lineage-masked even if its body contains the span.
    let span = "fn target() {}";
    let mut conversation = vec![
        LlmInputItem::FunctionCall {
            call_id: "g1".to_string(),
            name: "grep".to_string(),
            arguments: json!({ "pattern": "target" }),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "g1".to_string(),
            output: format!("foo.rs:1:{span}"),
            content_parts: None,
            is_error: false,
        },
    ];
    let edits = vec![SuccessfulEdit {
        path: "foo.rs".to_string(),
        changed_spans: vec![span.to_string()],
        whole_file: false,
    }];
    let report = mask_expired_reads_after_edits(&mut conversation, &edits, 0);
    assert!(
        report.is_none(),
        "a path-less repo-wide grep must not be lineage-masked",
    );
}

#[test]
fn mask_expired_is_idempotent() {
    let span = "fn target() { legacy(input, ctx, &mut sink, deprecated_flag_default_value); }";
    let body = format!("// a\n{span}\n// b");
    let mut conversation = read_pair("r1", "foo.rs", &body);
    let edits = vec![SuccessfulEdit {
        path: "foo.rs".to_string(),
        changed_spans: vec![span.to_string()],
        whole_file: false,
    }];
    let first = mask_expired_reads_after_edits(&mut conversation, &edits, 0);
    assert!(first.is_some(), "first pass masks the span");
    let second = mask_expired_reads_after_edits(&mut conversation, &edits, 0);
    assert!(
        second.is_none(),
        "the span is gone after the first pass; the second is a no-op",
    );
}
