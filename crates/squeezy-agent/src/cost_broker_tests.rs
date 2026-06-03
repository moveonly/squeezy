use super::*;
use serde_json::json;
use squeezy_tools::{ToolCostHint, ToolReceipt};

fn sample_status() -> CostCapStatus {
    CostCapStatus {
        spent_usd_micros: 12_457,
        cap_usd_micros: 10_000,
        percent: 124,
    }
}

#[test]
fn cap_reached_reason_states_spent_cap_and_percent() {
    let msg = format_cap_reached_reason(sample_status());
    assert!(
        msg.contains("$0.012457"),
        "spent amount must be cited; got: {msg}"
    );
    assert!(
        msg.contains("$0.010000"),
        "cap amount must be cited; got: {msg}"
    );
    assert!(msg.contains("(124%)"), "percent must be cited; got: {msg}");
}

#[test]
fn cap_reached_reason_includes_next_step_guidance() {
    // squeezy-zp6e: the cap-reached error must steer the user to a
    // concrete next step (raise the cap via /config or env var).
    // Without this the user is left with a bare numeric message and
    // no idea how to recover.
    let msg = format_cap_reached_reason(sample_status());
    assert!(
        msg.contains("/config"),
        "cap-reached message must reference /config; got: {msg}"
    );
    assert!(
        msg.contains("max_session_cost_usd_micros"),
        "cap-reached message must name the setting; got: {msg}"
    );
    assert!(
        msg.contains("SQUEEZY_MAX_SESSION_COST_USD_MICROS"),
        "cap-reached message must cite the env var override; got: {msg}"
    );
}

#[test]
fn warn_threshold_notice_includes_next_step_guidance() {
    // squeezy-zp6e: the warning-tier notice also needs an actionable
    // hint so the user can raise the cap *before* the hard cap trips
    // and a turn fails outright.
    let status = CostCapStatus {
        spent_usd_micros: 9_600,
        cap_usd_micros: 10_000,
        percent: 96,
    };
    let notice = format_warn_threshold_notice(status);
    assert!(
        notice.contains("warning threshold"),
        "notice must label itself as the warning tier; got: {notice}"
    );
    assert!(
        notice.contains("/config"),
        "warn notice must reference /config; got: {notice}"
    );
    assert!(
        notice.contains("max_session_cost_usd_micros"),
        "warn notice must name the setting; got: {notice}"
    );
    assert!(
        notice.contains("(96%)"),
        "warn notice must cite the percent; got: {notice}"
    );
}

fn config_with_cap(cap_micros: u64) -> AppConfig {
    AppConfig {
        max_session_cost_usd_micros: Some(cap_micros),
        ..AppConfig::default()
    }
}

#[test]
fn unenforceable_cap_round_signals_once_and_freezes_accumulator() {
    // A cap is configured but the round has no per-round dollar estimate
    // (no registry pricing for this model). The accumulator can't advance,
    // so neither the warning nor the hard cap can ever fire — surface a
    // single notice instead of silently no-op'ing the guardrail.
    let mut broker = CostBroker::new(&config_with_cap(10_000));
    let no_pricing = CostSnapshot {
        input_tokens: Some(50_000),
        output_tokens: Some(50_000),
        estimated_usd_micros: None,
        ..Default::default()
    };

    let cap_status = broker.record_provider_cost(&no_pricing);
    assert!(
        cap_status.is_none(),
        "no dollar estimate means no warning/cap event can fire"
    );
    assert_eq!(
        broker.session_cost_usd_micros, 0,
        "an unpriced round leaves the accumulator at 0"
    );
    assert!(
        broker.note_unenforceable_cap_round(&no_pricing),
        "first unpriced round under a cap must emit the cap-unenforceable signal"
    );
    assert!(
        !broker.note_unenforceable_cap_round(&no_pricing),
        "the cap-unenforceable signal is one-shot"
    );
}

#[test]
fn unenforceable_cap_signal_suppressed_without_cap_or_with_pricing() {
    // No cap configured: the cap can't be unenforceable, so stay silent.
    let mut no_cap = CostBroker::new(&AppConfig::default());
    let no_pricing = CostSnapshot {
        input_tokens: Some(1_000),
        estimated_usd_micros: None,
        ..Default::default()
    };
    assert!(
        !no_cap.note_unenforceable_cap_round(&no_pricing),
        "no cap means there is nothing to warn about"
    );

    // Cap configured and the round carries a dollar estimate: the cap is
    // enforceable, so no notice.
    let mut priced = CostBroker::new(&config_with_cap(10_000));
    let priced_round = CostSnapshot {
        input_tokens: Some(1_000),
        estimated_usd_micros: Some(2_000),
        ..Default::default()
    };
    assert!(
        !priced.note_unenforceable_cap_round(&priced_round),
        "a priced round keeps the cap enforceable"
    );
}

#[test]
fn cap_unenforceable_notice_names_provider_model_and_setting() {
    let notice = format_cap_unenforceable_notice("openrouter", "anthropic/claude-opus-4-7");
    assert!(
        notice.contains("openrouter/anthropic/claude-opus-4-7"),
        "notice must cite the provider/model; got: {notice}"
    );
    assert!(
        notice.contains("cannot be enforced"),
        "notice must state the cap is inert; got: {notice}"
    );
    assert!(
        notice.contains("max_session_cost_usd_micros"),
        "notice must name the setting; got: {notice}"
    );
}

#[test]
fn round_input_gate_off_when_limit_unset() {
    // Default-off: with `max_round_input_tokens == None` the gate returns
    // `None` no matter how large the estimate, so the round dispatches
    // unchanged.
    let status = round_input_gate_status(
        None,
        10_000_000,
        "anthropic",
        squeezy_core::DEFAULT_ANTHROPIC_MODEL,
        4_096,
    );
    assert!(status.is_none(), "an unset limit must never gate a round");
}

#[test]
fn round_input_gate_passes_when_under_or_at_limit() {
    // At or under the ceiling the gate stays quiet (the estimate is the
    // *projected* size, and the limit is inclusive).
    assert!(
        round_input_gate_status(
            Some(1_000),
            999,
            "anthropic",
            squeezy_core::DEFAULT_ANTHROPIC_MODEL,
            4_096,
        )
        .is_none(),
        "an under-limit estimate must not gate"
    );
    assert!(
        round_input_gate_status(
            Some(1_000),
            1_000,
            "anthropic",
            squeezy_core::DEFAULT_ANTHROPIC_MODEL,
            4_096,
        )
        .is_none(),
        "an estimate exactly at the limit must not gate"
    );
}

#[test]
fn round_input_gate_fires_with_priced_round_when_over_limit() {
    // Over the ceiling the gate fires and, because the model has registry
    // pricing, carries a non-zero dollar projection computed with the same
    // `estimate_cost` the session-cost cap uses.
    let status = round_input_gate_status(
        Some(1_000),
        50_000,
        "anthropic",
        squeezy_core::DEFAULT_ANTHROPIC_MODEL,
        4_096,
    )
    .expect("an over-limit estimate must gate");
    assert_eq!(status.estimated_input_tokens, 50_000);
    assert_eq!(status.limit_tokens, 1_000);
    let priced = status
        .estimated_usd_micros
        .expect("a priced model must yield a dollar projection");
    assert!(priced > 0, "the projected round must cost something");
    // The dollar figure must match a direct `estimate_cost` on the same
    // projection so the gate doesn't carry an independent cost model.
    let direct = estimate_cost(
        "anthropic",
        squeezy_core::DEFAULT_ANTHROPIC_MODEL,
        &CostSnapshot {
            input_tokens: Some(50_000),
            output_tokens: Some(4_096),
            ..Default::default()
        },
    )
    .expect("priced model");
    assert_eq!(
        priced, direct,
        "gate dollar figure must equal estimate_cost"
    );
}

#[test]
fn round_input_gate_fires_unpriced_when_model_unknown() {
    // Over the ceiling on a model with no registry pricing: the token gate
    // still fires (so an oversized round is still caught) but the dollar
    // field is `None` rather than a fabricated number.
    let status = round_input_gate_status(
        Some(1_000),
        50_000,
        "no-such-provider",
        "no-such-model",
        4_096,
    )
    .expect("the token gate fires regardless of pricing");
    assert_eq!(status.estimated_input_tokens, 50_000);
    assert!(
        status.estimated_usd_micros.is_none(),
        "an unpriced model must not invent a dollar figure"
    );
}

#[test]
fn round_input_gate_reason_states_overage_cost_and_setting() {
    let status = RoundInputGateStatus {
        estimated_input_tokens: 50_000,
        limit_tokens: 1_000,
        estimated_usd_micros: Some(123_456),
    };
    let msg = format_round_input_gate_reason(status);
    assert!(
        msg.contains("50000") && msg.contains("1000"),
        "message must cite estimate and ceiling; got: {msg}"
    );
    assert!(
        msg.contains("$0.1235"),
        "message must quote the projected round cost; got: {msg}"
    );
    assert!(
        msg.contains("max_round_input_tokens"),
        "message must name the setting; got: {msg}"
    );
    assert!(
        msg.contains("SQUEEZY_MAX_ROUND_INPUT_TOKENS"),
        "message must cite the env override; got: {msg}"
    );

    // Unpriced rounds omit the dollar clause but keep the gate guidance.
    let unpriced = format_round_input_gate_reason(RoundInputGateStatus {
        estimated_input_tokens: 50_000,
        limit_tokens: 1_000,
        estimated_usd_micros: None,
    });
    assert!(
        !unpriced.contains('$'),
        "an unpriced gate must not quote a dollar figure; got: {unpriced}"
    );
    assert!(
        unpriced.contains("max_round_input_tokens"),
        "an unpriced gate still names the setting; got: {unpriced}"
    );
}

#[test]
fn budget_denied_result_counts_once_across_accounting_paths() {
    let mut broker = CostBroker::new(&AppConfig::default());
    let result = ToolResult {
        call_id: "call-1".to_string(),
        tool_name: "read_file".to_string(),
        status: ToolStatus::Denied,
        content: json!({
            "budget_denied": true,
            "error": "budget exhausted",
        }),
        cost_hint: ToolCostHint::default(),
        receipt: ToolReceipt {
            output_sha256: "sha".to_string(),
            content_sha256: None,
        },
        spill_model_output: None,
    };

    broker.record_executed_result(&result);
    broker.record_model_result(&result);

    assert_eq!(broker.metrics.budget_denials, 1);
}
