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
