use futures_util::StreamExt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::*;

#[tokio::test]
async fn unavailable_provider_reports_configuration_error() {
    let provider = UnavailableProvider::new("openai", "missing OPENAI_API_KEY");
    let request = LlmRequest {
        model: "test-model".to_string().into(),
        instructions: "test".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: Some(16),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let mut stream = provider.stream_response(request, CancellationToken::new());
    let err = stream.next().await.expect("one event").expect_err("error");

    assert!(err.to_string().contains("missing OPENAI_API_KEY"));
    assert!(stream.next().await.is_none());
}

#[test]
fn registry_estimates_known_model_costs() {
    let cost = CostSnapshot {
        input_tokens: Some(1_000_000),
        output_tokens: Some(1_000_000),
        reasoning_output_tokens: None,
        cached_input_tokens: Some(1_000_000),
        cache_write_input_tokens: None,
        estimated_usd_micros: None,
    };

    let estimate = estimate_cost("openai", squeezy_core::DEFAULT_OPENAI_MODEL, &cost);

    assert_eq!(estimate, Some(30_500_000));
}

#[test]
fn registry_estimate_costs_anthropic_with_normalised_input_tokens() {
    // `CostSnapshot.input_tokens` is the **total** prompt the model
    // saw (uncached + cache_read + cache_write) under the normalised
    // cross-provider convention. The Anthropic and Bedrock stream
    // states fold the cache counters back in at the snapshot
    // boundary so `estimate_cost` can run a single subtraction without
    // a per-provider branch.
    //
    // Equivalent snapshots: 200 standard tokens, with vs. without a
    // 5_000-token cache hit. Cache pricing is additive, so the cached
    // estimate must be >= the uncached one.
    let cached = CostSnapshot {
        input_tokens: Some(5_200),
        output_tokens: Some(50),
        reasoning_output_tokens: None,
        cached_input_tokens: Some(5_000),
        cache_write_input_tokens: Some(0),
        estimated_usd_micros: None,
    };
    let uncached = CostSnapshot {
        input_tokens: Some(200),
        output_tokens: Some(50),
        reasoning_output_tokens: None,
        cached_input_tokens: Some(0),
        cache_write_input_tokens: Some(0),
        estimated_usd_micros: None,
    };
    let cached_estimate =
        estimate_cost("anthropic", squeezy_core::DEFAULT_ANTHROPIC_MODEL, &cached);
    let uncached_estimate = estimate_cost(
        "anthropic",
        squeezy_core::DEFAULT_ANTHROPIC_MODEL,
        &uncached,
    );

    // The 200 standard input tokens must still be billed at the standard rate
    // even when cache_read is large; the only delta should be the cache_read
    // surcharge.
    assert!(cached_estimate.is_some());
    assert!(uncached_estimate.is_some());
    assert!(
        cached_estimate.unwrap() >= uncached_estimate.unwrap(),
        "cached cost {:?} must be >= uncached cost {:?} (cache_read is additive, not a discount)",
        cached_estimate,
        uncached_estimate,
    );
}

#[test]
fn registry_lists_ollama_as_zero_cost_local_provider() {
    let model = models_for_provider("ollama").next().expect("ollama model");

    assert_eq!(model.provider, "ollama");
    assert_eq!(model.pricing.unwrap().input_usd_micros_per_mtok, 0);
    assert!(model.capabilities.streaming);
}

#[test]
fn effective_cache_helpers_match_legacy_bridge_rules() {
    let mut request = LlmRequest::user_text(
        "gpt-5.1".to_string(),
        "system".to_string(),
        "hello".to_string(),
        None,
    );

    assert_eq!(request.effective_cache_key(), None);
    assert_eq!(request.effective_cache_retention(), CacheRetention::None);

    request.cache_key = Some("legacy-session".to_string());
    assert_eq!(request.effective_cache_key(), Some("legacy-session"));
    assert_eq!(request.effective_cache_retention(), CacheRetention::Short);

    request.cache = CacheSpec {
        key: Some("explicit-session".to_string()),
        retention: CacheRetention::Long,
    };
    assert_eq!(request.effective_cache_key(), Some("explicit-session"));
    assert_eq!(request.effective_cache_retention(), CacheRetention::Long);

    let spec = request.effective_cache_spec();
    assert_eq!(spec.key.as_deref(), Some("explicit-session"));
    assert_eq!(spec.retention, CacheRetention::Long);
}

#[test]
fn registry_lists_context_limits_for_hosted_defaults() {
    let openai = model_info_for("openai", squeezy_core::DEFAULT_OPENAI_MODEL).expect("openai");
    assert_eq!(openai.limits.unwrap().context_window_tokens, 400_000);
    assert_eq!(openai.limits.unwrap().max_output_tokens, 128_000);

    let anthropic =
        model_info_for("anthropic", squeezy_core::DEFAULT_ANTHROPIC_MODEL).expect("anthropic");
    assert_eq!(squeezy_core::DEFAULT_ANTHROPIC_MODEL, "claude-sonnet-4-6");
    assert_eq!(anthropic.limits.unwrap().context_window_tokens, 200_000);
    assert_eq!(anthropic.limits.unwrap().max_output_tokens, 64_000);

    let bedrock = model_info_for("bedrock", squeezy_core::DEFAULT_BEDROCK_MODEL).expect("bedrock");
    assert_eq!(
        squeezy_core::DEFAULT_BEDROCK_MODEL,
        "anthropic.claude-sonnet-4-6"
    );
    assert_eq!(bedrock.limits.unwrap().context_window_tokens, 1_000_000);

    let google = model_info_for("google", squeezy_core::DEFAULT_GOOGLE_MODEL).expect("google");
    assert_eq!(google.limits.unwrap().context_window_tokens, 1_048_576);

    let vertex_flash =
        model_info_for("vertex", squeezy_core::VERTEX_SMALL_FAST_MODEL).expect("vertex flash");
    assert_eq!(
        vertex_flash.limits.unwrap().context_window_tokens,
        1_048_576
    );

    let ollama = model_info_for("ollama", squeezy_core::DEFAULT_OLLAMA_MODEL).expect("ollama");
    assert!(ollama.limits.is_none());
}

#[test]
fn registry_lists_three_tiers_for_major_hosted_providers() {
    for provider in ["openai", "anthropic", "google"] {
        let models = models_for_provider(provider).collect::<Vec<_>>();
        assert!(
            models.len() >= 3,
            "{provider} should expose at least three selectable models"
        );
        assert!(
            models
                .iter()
                .any(|model| model.profile == squeezy_core::ModelProfile::Strong)
        );
        assert!(
            models
                .iter()
                .any(|model| model.profile == squeezy_core::ModelProfile::Balanced)
        );
        assert!(
            models
                .iter()
                .any(|model| model.profile == squeezy_core::ModelProfile::Cheap)
        );
    }
}

#[test]
fn request_context_estimate_reports_budget_when_model_limit_exists() {
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_OPENAI_MODEL.to_string().into(),
        instructions: "short system prompt".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: Some(128),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let estimate =
        estimate_request_context("openai", squeezy_core::DEFAULT_OPENAI_MODEL, &request, None);

    assert!(estimate.input_tokens > 0);
    assert_eq!(estimate.context_window_tokens, Some(400_000));
    // 95% of the raw window, minus a fixed 12_000-token baseline reserved
    // for system framing.
    assert_eq!(estimate.effective_context_window_tokens, Some(368_000));
    // Headroom = raw window - effective window.
    assert_eq!(estimate.headroom_tokens, Some(32_000));
    assert_eq!(estimate.max_output_tokens, Some(128));
    // Effective window minus max_output_tokens.
    assert_eq!(estimate.input_budget_tokens, Some(367_872));
    assert!(estimate.remaining_input_tokens.unwrap() < 367_872);
    assert!(estimate.used_input_percent_x100.is_some());
}

#[test]
fn calibrated_request_context_estimate_uses_provided_bytes_per_token() {
    // The same request must produce fewer estimated input tokens when we hand
    // in a calibration with a *higher* bytes/token ratio: the estimator
    // divides bytes by the ratio, so a bigger ratio means fewer tokens. This
    // is the contract rjr.105 relies on - a calibrated session that learns
    // its provider packs more bytes per token shows a smaller projected
    // input usage.
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_OPENAI_MODEL.to_string().into(),
        instructions: "a moderately long system prompt with enough text to estimate"
            .to_string()
            .into(),
        input: Arc::from(vec![LlmInputItem::UserText(
            "another moderately long user message with several words".to_string(),
        )]),
        max_output_tokens: Some(128),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let default_estimate =
        estimate_request_context("openai", squeezy_core::DEFAULT_OPENAI_MODEL, &request, None);

    // Seed a calibration claiming each token costs *eight* bytes - double
    // the default 4.0 - so the estimator should report roughly half the
    // input tokens.
    let mut calibration = crate::tokens::TokenCalibration::default();
    calibration.record_sample("openai", 8000, 1000);
    let calibrated_estimate = estimate_request_context_calibrated(
        "openai",
        squeezy_core::DEFAULT_OPENAI_MODEL,
        &request,
        None,
        Some(&calibration),
    );

    assert!(
        calibrated_estimate.input_tokens < default_estimate.input_tokens,
        "calibrated estimate ({}) must be smaller than default ({})",
        calibrated_estimate.input_tokens,
        default_estimate.input_tokens,
    );
    assert!(
        calibrated_estimate.input_tokens > 0,
        "calibrated estimate should still cover the structural overhead"
    );
}

#[test]
fn normalize_tool_ids_rewrites_paired_call_and_output_to_canonical_form() {
    // Paired FunctionCall + FunctionCallOutput with a provider-specific id
    // (here the long OpenAI Responses shape) get rewritten to `call_1`
    // *and* both sides see the same canonical id so the destination
    // provider can still pair them. Without this an Anthropic destination
    // would reject the original id outright (regex + length cap).
    let openai_responses_id = "fc_abcdEFGHijklMNOP1234567890|qrSTuvWXyzABC|DEFghijklmnop";
    let normalized = normalize_tool_ids_for_replay(&[
        LlmInputItem::FunctionCall {
            call_id: openai_responses_id.to_string(),
            name: "grep".to_string(),
            arguments: serde_json::json!({"pattern": "todo"}),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: openai_responses_id.to_string(),
            output: "match".to_string(),
            content_parts: None,
            is_error: false,
        },
    ]);

    assert_eq!(normalized.len(), 2, "no synthetic placeholder when paired");
    match &normalized[0] {
        LlmInputItem::FunctionCall {
            call_id,
            name,
            arguments,
        } => {
            assert_eq!(call_id, "call_1");
            assert_eq!(name, "grep");
            assert_eq!(arguments, &serde_json::json!({"pattern": "todo"}));
        }
        other => panic!("expected FunctionCall, got {other:?}"),
    }
    match &normalized[1] {
        LlmInputItem::FunctionCallOutput {
            call_id, output, ..
        } => {
            assert_eq!(call_id, "call_1");
            assert_eq!(output, "match");
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn normalize_tool_ids_assigns_distinct_canonical_ids_per_call() {
    // Two distinct tool turns produce two distinct canonical ids in
    // first-seen order; each result tracks back to the right call.
    let normalized = normalize_tool_ids_for_replay(&[
        LlmInputItem::FunctionCall {
            call_id: "toolu_abc".to_string(),
            name: "read".to_string(),
            arguments: serde_json::Value::Object(Default::default()),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "toolu_abc".to_string(),
            output: "ok".to_string(),
            content_parts: None,
            is_error: false,
        },
        LlmInputItem::FunctionCall {
            call_id: "google_call_2".to_string(),
            name: "write".to_string(),
            arguments: serde_json::Value::Object(Default::default()),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "google_call_2".to_string(),
            output: "ok".to_string(),
            content_parts: None,
            is_error: false,
        },
    ]);

    let call_ids: Vec<&str> = normalized
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::FunctionCall { call_id, .. }
            | LlmInputItem::FunctionCallOutput { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(call_ids, vec!["call_1", "call_1", "call_2", "call_2"]);
}

#[test]
fn normalize_tool_ids_synthesizes_placeholder_for_orphan_tool_result() {
    // An orphan tool result (call_id not introduced by a prior
    // FunctionCall in the same slice) gets a synthesized
    // `model_switched` FunctionCall inserted ahead of it so the
    // destination provider sees a well-formed pairing.
    let normalized = normalize_tool_ids_for_replay(&[
        LlmInputItem::UserText("look it up".to_string()),
        LlmInputItem::FunctionCallOutput {
            call_id: "lost_after_model_swap".to_string(),
            output: "result".to_string(),
            content_parts: None,
            is_error: false,
        },
        LlmInputItem::UserText("now what?".to_string()),
    ]);

    assert_eq!(
        normalized.len(),
        4,
        "user + synthetic call + tool result + user"
    );
    assert!(matches!(&normalized[0], LlmInputItem::UserText(t) if t == "look it up"));
    match &normalized[1] {
        LlmInputItem::FunctionCall {
            call_id,
            name,
            arguments,
        } => {
            assert_eq!(call_id, "call_1");
            assert_eq!(name, MODEL_SWITCHED_PLACEHOLDER_NAME);
            assert_eq!(arguments, &serde_json::json!({"reason": "model_switched"}));
        }
        other => panic!("expected synthesized FunctionCall, got {other:?}"),
    }
    match &normalized[2] {
        LlmInputItem::FunctionCallOutput {
            call_id, output, ..
        } => {
            assert_eq!(call_id, "call_1");
            assert_eq!(output, "result");
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
    assert!(matches!(&normalized[3], LlmInputItem::UserText(t) if t == "now what?"));
}

#[test]
fn normalize_tool_ids_pairs_orphan_synthesis_with_subsequent_real_calls() {
    // Mixed history: an orphan tool result followed by a real
    // call/result pair. The synthetic placeholder takes id `call_1`,
    // the genuine pair takes the next available canonical id
    // (`call_2`). This is the realistic mid-session model-switch
    // shape — the prior model's turn was lost but the new model's
    // subsequent tool round-trip still flows cleanly.
    let normalized = normalize_tool_ids_for_replay(&[
        LlmInputItem::FunctionCallOutput {
            call_id: "fc_orphan".to_string(),
            output: "stale".to_string(),
            content_parts: None,
            is_error: false,
        },
        LlmInputItem::FunctionCall {
            call_id: "toolu_new".to_string(),
            name: "grep".to_string(),
            arguments: serde_json::json!({"pattern": "x"}),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "toolu_new".to_string(),
            output: "match".to_string(),
            content_parts: None,
            is_error: false,
        },
    ]);

    let ids: Vec<&str> = normalized
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::FunctionCall { call_id, .. }
            | LlmInputItem::FunctionCallOutput { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ids,
        vec!["call_1", "call_1", "call_2", "call_2"],
        "synthetic placeholder gets call_1 paired with the orphan output; the genuine pair takes call_2",
    );

    // The synthesized placeholder carries the model_switched marker
    // so review tooling can distinguish it from a real call the
    // model issued.
    let names: Vec<&str> = normalized
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::FunctionCall { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec![MODEL_SWITCHED_PLACEHOLDER_NAME, "grep"]);
}

#[test]
fn normalize_tool_ids_passes_user_assistant_and_reasoning_items_through() {
    // Non-tool items must not be mutated or dropped — the
    // normalization is additive on the tool-call surface only.
    let reasoning = LlmInputItem::Reasoning(ReasoningPayload::OpenAi {
        item_id: "rs_1".to_string(),
        summary: vec!["thinking".to_string()],
        encrypted_content: None,
    });
    let normalized = normalize_tool_ids_for_replay(&[
        LlmInputItem::UserText("u".to_string()),
        LlmInputItem::AssistantText("a".to_string()),
        reasoning.clone(),
    ]);
    assert_eq!(normalized.len(), 3);
    assert!(matches!(&normalized[0], LlmInputItem::UserText(t) if t == "u"));
    assert!(matches!(&normalized[1], LlmInputItem::AssistantText(t) if t == "a"));
    assert_eq!(&normalized[2], &reasoning);
}

#[test]
fn normalize_tool_ids_is_idempotent_on_already_canonical_input() {
    // Re-running normalization on a slice that already uses the
    // canonical `call_<N>` shape yields the same slice. This matters
    // because each provider's `request_body` calls the helper on
    // every turn — a persisted-then-replayed history must not be
    // re-numbered every time it flows through the request path.
    let canonical = vec![
        LlmInputItem::FunctionCall {
            call_id: "call_1".to_string(),
            name: "grep".to_string(),
            arguments: serde_json::json!({"pattern": "x"}),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "call_1".to_string(),
            output: "hit".to_string(),
            content_parts: None,
            is_error: false,
        },
        LlmInputItem::FunctionCall {
            call_id: "call_2".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({"path": "p"}),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "call_2".to_string(),
            output: "data".to_string(),
            content_parts: None,
            is_error: false,
        },
    ];
    let once = normalize_tool_ids_for_replay(&canonical);
    let twice = normalize_tool_ids_for_replay(&once);
    assert_eq!(
        once, canonical,
        "first pass leaves canonical input unchanged"
    );
    assert_eq!(twice, once, "second pass is a no-op on canonical input");
}

#[test]
fn normalize_tool_ids_simulates_anthropic_to_openai_cross_model_replay() {
    // End-to-end replay shape: a session that started on Anthropic
    // (toolu_* ids), got interrupted before the tool result returned
    // (synthetic placeholder needed), then continued on OpenAI
    // (fc_*|… ids). The normalized slice has consistent canonical
    // ids the destination Anthropic provider would accept (regex +
    // 64-char cap) AND every output has a matching call ahead of
    // it.
    let openai_id = "fc_long_responses_id_with|pipe_separator_well_past_64_chars_aaa";
    let normalized = normalize_tool_ids_for_replay(&[
        LlmInputItem::UserText("start".to_string()),
        // Orphaned Anthropic-shaped result — the assistant turn that
        // would have produced toolu_aaa was discarded mid-session.
        LlmInputItem::FunctionCallOutput {
            call_id: "toolu_aaa".to_string(),
            output: "(stale)".to_string(),
            content_parts: None,
            is_error: false,
        },
        // Subsequent OpenAI Responses turn with its native id shape.
        LlmInputItem::FunctionCall {
            call_id: openai_id.to_string(),
            name: "search".to_string(),
            arguments: serde_json::json!({"q": "rust"}),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: openai_id.to_string(),
            output: "found".to_string(),
            content_parts: None,
            is_error: false,
        },
    ]);

    assert_eq!(
        normalized.len(),
        5,
        "user + synthesized placeholder + orphan output + real call + real output"
    );
    for item in &normalized {
        if let LlmInputItem::FunctionCall { call_id, .. }
        | LlmInputItem::FunctionCallOutput { call_id, .. } = item
        {
            // 64-char + regex compliance: every canonical id is short
            // and matches `^call_[0-9]+$`.
            assert!(
                call_id.len() <= 64,
                "call_id {call_id} must fit Anthropic 64-char cap"
            );
            assert!(
                call_id
                    .strip_prefix("call_")
                    .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit())),
                "call_id {call_id} must match `call_<N>` canonical form",
            );
        }
    }
}

#[test]
fn request_context_estimate_uses_fallback_metadata_for_unknown_models() {
    // The bundled registry now ships a fallback metadata path so unknown
    // model ids still get useful headroom/budget figures instead of empty
    // optionals.
    let request = LlmRequest {
        model: "custom-model".to_string().into(),
        instructions: "system".to_string().into(),
        input: Arc::from(Vec::<LlmInputItem>::new()),
        max_output_tokens: Some(128),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let estimate = estimate_request_context("openai", "custom-model", &request, None);

    assert!(estimate.input_tokens > 0);
    assert_eq!(estimate.context_window_tokens, Some(272_000));
    assert!(estimate.effective_context_window_tokens.unwrap() < 272_000);
    assert!(estimate.headroom_tokens.unwrap() > 0);
    assert!(estimate.input_budget_tokens.unwrap() > 0);
    assert!(estimate.remaining_input_tokens.is_some());
    assert!(estimate.used_input_percent_x100.is_some());
}

#[test]
fn infer_image_mime_detects_canonical_magic_numbers() {
    let png: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];
    assert_eq!(infer_image_mime(png), Some("image/png"));

    let jpeg: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    assert_eq!(infer_image_mime(jpeg), Some("image/jpeg"));

    let gif87a: &[u8] = b"GIF87a\x00\x00";
    assert_eq!(infer_image_mime(gif87a), Some("image/gif"));

    let gif89a: &[u8] = b"GIF89a\x00\x00";
    assert_eq!(infer_image_mime(gif89a), Some("image/gif"));

    let mut webp = Vec::with_capacity(20);
    webp.extend_from_slice(b"RIFF");
    webp.extend_from_slice(&[0x10, 0x00, 0x00, 0x00]);
    webp.extend_from_slice(b"WEBPVP8 ");
    assert_eq!(infer_image_mime(&webp), Some("image/webp"));

    // Non-image bytes don't match.
    assert_eq!(infer_image_mime(b"plain text content"), None);
    assert_eq!(infer_image_mime(&[]), None);
}

#[test]
fn ensure_vision_support_rejects_text_only_model() {
    let png_bytes: Arc<[u8]> = Arc::from(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let request = LlmRequest {
        model: "deepseek-chat".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::UserText("describe this".to_string()),
            LlmInputItem::Image {
                media_type: "image/png".to_string(),
                bytes: png_bytes,
            },
        ]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,

        cache: crate::CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let err = request
        .ensure_vision_support("deepseek")
        .expect_err("text-only model must refuse image inputs");
    let message = err.to_string();
    assert!(
        message.contains("does not support image inputs"),
        "error must explain the rejection: got {message}"
    );
    assert!(
        message.contains("deepseek-chat"),
        "error must mention the rejected model id: got {message}"
    );
}

#[test]
fn ensure_vision_support_accepts_vision_capable_model() {
    let png_bytes: Arc<[u8]> = Arc::from(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::Image {
            media_type: "image/png".to_string(),
            bytes: png_bytes,
        }]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,

        cache: crate::CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    request
        .ensure_vision_support("anthropic")
        .expect("vision-capable model must accept image inputs");
}

#[test]
fn ensure_vision_support_is_noop_for_text_only_request() {
    let request = LlmRequest {
        model: "deepseek-chat".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,

        cache: crate::CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    request
        .ensure_vision_support("deepseek")
        .expect("text-only request must skip the vision check");
}

#[test]
fn reject_unsupported_documents_errors_with_document_metadata() {
    let request = LlmRequest {
        model: "gpt-5.5".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::UserText("summarize this".to_string()),
            LlmInputItem::Document {
                media_type: "application/pdf".to_string(),
                name: "report.pdf".to_string(),
                bytes: Arc::from(b"%PDF".as_slice()),
            },
        ]),
        ..LlmRequest::default()
    };

    let err = request
        .reject_unsupported_documents("openai")
        .expect_err("unsupported provider must reject document inputs");
    let message = err.to_string();
    assert!(
        message.contains("does not support document inputs"),
        "error must explain the rejection: got {message}"
    );
    assert!(
        message.contains("report.pdf") && message.contains("application/pdf"),
        "error must name the rejected document and media type: got {message}"
    );
}

#[test]
fn reject_unsupported_documents_is_noop_without_documents() {
    let request = LlmRequest {
        model: "gpt-5.5".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        ..LlmRequest::default()
    };

    request
        .reject_unsupported_documents("openai")
        .expect("text-only request must skip the document check");
}

#[test]
fn llm_input_item_image_round_trips_through_serde() {
    let original = LlmInputItem::Image {
        media_type: "image/png".to_string(),
        bytes: Arc::from(vec![1u8, 2, 3, 4, 5, 6, 7, 8]),
    };
    let json = serde_json::to_string(&original).expect("serialize image item");
    // The wire form stores bytes as a base64 string, not a byte array,
    // so a JSON checkpoint stays compact and human-debuggable.
    assert!(
        json.contains("\"AQIDBAUGBwg=\""),
        "image bytes must serialize as base64: {json}"
    );
    let decoded: LlmInputItem = serde_json::from_str(&json).expect("deserialize image item");
    assert_eq!(original, decoded);
}

#[test]
fn function_call_output_defaults_missing_optional_fields() {
    // A persisted/checkpoint payload written before the structured
    // tool-result extensions landed omits both `content_parts` and
    // `is_error`. Deserialization must default them (None / false) so
    // old transcripts stay loadable.
    let legacy = r#"{
        "type": "function_call_output",
        "data": { "call_id": "call_1", "output": "result text" }
    }"#;
    let decoded: LlmInputItem =
        serde_json::from_str(legacy).expect("legacy function_call_output must deserialize");
    match decoded {
        LlmInputItem::FunctionCallOutput {
            call_id,
            output,
            content_parts,
            is_error,
        } => {
            assert_eq!(call_id, "call_1");
            assert_eq!(output, "result text");
            assert!(
                content_parts.is_none(),
                "missing content_parts must default to None"
            );
            assert!(!is_error, "missing is_error must default to false");
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn document_round_trips_bytes_as_base64_and_preserves_metadata() {
    let original = LlmInputItem::Document {
        media_type: "application/pdf".to_string(),
        name: "report.pdf".to_string(),
        bytes: Arc::from(vec![1u8, 2, 3, 4, 5, 6, 7, 8]),
    };
    let json = serde_json::to_string(&original).expect("serialize document item");
    // Bytes ride the wire as a compact base64 string, never a JSON byte
    // array; the human-facing `name` and MIME type stay intact.
    assert!(
        json.contains("\"AQIDBAUGBwg=\""),
        "document bytes must serialize as base64: {json}"
    );
    assert!(
        json.contains("\"report.pdf\""),
        "document name must survive: {json}"
    );
    assert!(
        json.contains("\"application/pdf\""),
        "document media_type must survive: {json}"
    );
    let decoded: LlmInputItem = serde_json::from_str(&json).expect("deserialize document item");
    assert_eq!(original, decoded);
}

#[test]
fn tool_result_part_maps_text_and_image_tags() {
    let text = ToolResultPart::Text {
        text: "ok".to_string(),
    };
    let text_json = serde_json::to_string(&text).expect("serialize text part");
    assert!(
        text_json.contains("\"type\":\"text\""),
        "Text part must use snake_case `text` tag: {text_json}"
    );
    assert_eq!(
        text,
        serde_json::from_str::<ToolResultPart>(&text_json).expect("deserialize text part")
    );

    let image = ToolResultPart::Image {
        media_type: "image/png".to_string(),
        bytes: Arc::from(vec![1u8, 2, 3, 4, 5, 6, 7, 8]),
    };
    let image_json = serde_json::to_string(&image).expect("serialize image part");
    assert!(
        image_json.contains("\"type\":\"image\""),
        "Image part must use snake_case `image` tag: {image_json}"
    );
    assert!(
        image_json.contains("\"AQIDBAUGBwg=\""),
        "Image part bytes must serialize as base64: {image_json}"
    );
    assert_eq!(
        image,
        serde_json::from_str::<ToolResultPart>(&image_json).expect("deserialize image part")
    );
}
