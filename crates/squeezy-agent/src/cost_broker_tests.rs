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

    let cap_status = broker.record_provider_cost(
        "anthropic",
        "claude-haiku-4-5-20251001",
        CostOrigin::Main,
        &no_pricing,
    );
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
fn pressure_percent_is_none_without_cap_and_tracks_spend_with_cap() {
    // No cap configured: there is nothing to be a percent of.
    let mut no_cap = CostBroker::new(&AppConfig::default());
    no_cap.session_cost_usd_micros = 50_000;
    assert_eq!(
        no_cap.pressure_percent(),
        None,
        "pressure has no meaning without a configured cap"
    );

    // Cap configured: pressure is spent/cap as a clamped percent.
    let mut broker = CostBroker::new(&config_with_cap(10_000));
    assert_eq!(
        broker.pressure_percent(),
        Some(0),
        "a fresh broker under a cap is at 0% pressure"
    );
    broker.session_cost_usd_micros = 5_000;
    assert_eq!(broker.pressure_percent(), Some(50));
    broker.session_cost_usd_micros = 7_999;
    assert_eq!(broker.pressure_percent(), Some(79));
    broker.session_cost_usd_micros = 8_000;
    assert_eq!(broker.pressure_percent(), Some(80));
    // Overshoot past the cap stays a valid percent (clamped, no overflow).
    broker.session_cost_usd_micros = 30_000;
    assert_eq!(broker.pressure_percent(), Some(255));
}

#[test]
fn pressure_gate_engages_at_threshold_when_cap_set() {
    let mut broker = CostBroker::new(&config_with_cap(10_000));
    // Just under 80%: no gate.
    broker.session_cost_usd_micros = 7_999;
    assert!(
        broker.pressure_gate().is_none(),
        "below the pressure threshold the gate must stay open"
    );
    // At exactly 80%: gate engages and reports the pressure status.
    broker.session_cost_usd_micros = 8_000;
    let status = broker
        .pressure_gate()
        .expect("gate must engage at the pressure threshold");
    assert_eq!(status.spent_usd_micros, 8_000);
    assert_eq!(status.cap_usd_micros, 10_000);
    assert_eq!(status.percent, 80);
}

#[test]
fn pressure_gate_engages_above_threshold_and_is_one_shot() {
    let mut broker = CostBroker::new(&config_with_cap(10_000));
    // Well above the threshold (but the hard cap is a separate check): gate fires.
    broker.session_cost_usd_micros = 9_500;
    assert!(
        broker.pressure_gate().is_some(),
        "the gate engages once spend is at or past the pressure threshold"
    );
    // One-shot latch: it does not re-fire on subsequent rounds even as spend climbs.
    broker.session_cost_usd_micros = 9_900;
    assert!(
        broker.pressure_gate().is_none(),
        "the pressure gate is one-shot per broker"
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
fn pressure_gate_never_engages_without_cap() {
    let mut broker = CostBroker::new(&AppConfig::default());
    // Arbitrary large spend: with no cap there is no pressure to govern.
    broker.session_cost_usd_micros = 1_000_000_000;
    assert!(
        broker.pressure_gate().is_none(),
        "with no configured cap the pressure gate must never engage"
    );
    assert!(
        broker.pressure_gate().is_none(),
        "repeat call with no cap also stays open"
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
fn pressure_gate_does_not_engage_below_threshold() {
    let mut broker = CostBroker::new(&config_with_cap(10_000));
    broker.session_cost_usd_micros = 5_000;
    assert!(
        broker.pressure_gate().is_none(),
        "at 50% pressure the gate stays open and behaviour is unchanged"
    );
    assert_eq!(
        broker.pressure_percent(),
        Some(50),
        "reading pressure must not arm the gate latch"
    );
    // Crossing the threshold afterwards still fires (latch was not consumed below threshold).
    broker.session_cost_usd_micros = 8_500;
    assert!(
        broker.pressure_gate().is_some(),
        "a sub-threshold gate check must not consume the one-shot latch"
    );
}

#[test]
fn pressure_gate_reason_states_spend_cap_percent_and_next_step() {
    let status = CostCapStatus {
        spent_usd_micros: 8_000,
        cap_usd_micros: 10_000,
        percent: 80,
    };
    let msg = format_pressure_gate_reason(status);
    assert!(
        msg.contains("approaching cap"),
        "reason must frame this as a proactive pressure stop; got: {msg}"
    );
    assert!(
        msg.contains("$0.008000"),
        "reason must cite the spent amount; got: {msg}"
    );
    assert!(
        msg.contains("$0.010000"),
        "reason must cite the cap amount; got: {msg}"
    );
    assert!(
        msg.contains("(80%)"),
        "reason must cite the percent; got: {msg}"
    );
    assert!(
        msg.contains("/config"),
        "reason must reference /config; got: {msg}"
    );
    assert!(
        msg.contains("max_session_cost_usd_micros"),
        "reason must name the setting; got: {msg}"
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
        web_call_stats: None,
    };

    broker.record_executed_result(&result);
    broker.record_model_result(&result);

    assert_eq!(broker.metrics.budget_denials, 1);
}

#[test]
fn record_provider_cost_populates_per_model_ledger_without_drift() {
    let mut broker = CostBroker::new(&AppConfig::default());
    broker.record_provider_cost(
        "anthropic",
        "opus",
        CostOrigin::Main,
        &CostSnapshot {
            input_tokens: Some(100),
            estimated_usd_micros: Some(500),
            ..Default::default()
        },
    );
    broker.record_provider_cost(
        "anthropic",
        "haiku",
        CostOrigin::Subagent,
        &CostSnapshot {
            input_tokens: Some(20),
            estimated_usd_micros: Some(30),
            ..Default::default()
        },
    );

    let models: Vec<String> = broker
        .metrics
        .model_ledger
        .iter()
        .map(|b| b.model.clone())
        .collect();
    assert!(models.contains(&"opus".to_string()));
    assert!(models.contains(&"haiku".to_string()));

    // The opus round was main-origin, the haiku round subagent-origin.
    let opus = broker
        .metrics
        .model_ledger
        .iter()
        .find(|b| b.model == "opus")
        .unwrap();
    assert_eq!(opus.main.estimated_usd_micros, Some(500));
    assert_eq!(opus.subagent.estimated_usd_micros, None);
    let haiku = broker
        .metrics
        .model_ledger
        .iter()
        .find(|b| b.model == "haiku")
        .unwrap();
    assert_eq!(haiku.subagent.estimated_usd_micros, Some(30));
    assert_eq!(haiku.main.estimated_usd_micros, None);

    // The ledger total never drifts from the flat aggregate or the running
    // session snapshot — they all sum the same recorded rounds.
    assert_eq!(
        broker.metrics.model_ledger.totals().estimated_usd_micros,
        Some(530)
    );
    assert_eq!(broker.metrics.provider.estimated_usd_micros, Some(530));
    assert_eq!(
        broker.session_cost_snapshot().estimated_usd_micros,
        Some(530)
    );
}

#[test]
fn record_out_of_band_session_cost_advances_cap_basis_and_snapshot() {
    // Verifies that reviewer cost folded in via record_out_of_band_session_cost
    // is reflected in both the cap-basis total (used by cap checks) and the
    // session_cost_snapshot (used by the live status line), without affecting
    // the model ledger or turn metrics (those are already correct from the
    // direct state.cost update on the permission path).
    let config = squeezy_core::AppConfig {
        max_session_cost_usd_micros: Some(1_000_000),
        ..Default::default()
    };
    let mut broker = CostBroker::new(&config);
    let prior_cost = squeezy_core::CostSnapshot {
        estimated_usd_micros: Some(200_000),
        ..Default::default()
    };
    broker.seed_session(&prior_cost, squeezy_llm::TokenCalibration::default());

    // Simulate a reviewer call costing 5_000 micros.
    broker.record_out_of_band_session_cost(5_000);

    // The cap-basis total (used by session_cap_reached / projected_session_cap_overrun
    // / session_cost_snapshot) must advance.
    assert_eq!(broker.session_cost_usd_micros, 205_000);
    // session_cost_snapshot unconditionally returns session_cost_usd_micros as
    // the dollar field, so the snapshot also reflects the new total.
    assert_eq!(
        broker.session_cost_snapshot().estimated_usd_micros,
        Some(205_000)
    );

    // The model ledger and turn metrics must not be affected (they are
    // managed separately by the permission path's direct state.cost update).
    assert!(broker.metrics.model_ledger.is_empty());
    assert_eq!(broker.metrics.provider.estimated_usd_micros, None);

    // A zero-micros call is a safe no-op.
    broker.record_out_of_band_session_cost(0);
    assert_eq!(broker.session_cost_usd_micros, 205_000);
}

#[test]
fn record_out_of_band_session_cost_does_not_fire_cost_warning_then_next_record_provider_cost_does()
{
    // Lock the documented one-round lag (PR #403 review Nit #4).
    // `record_out_of_band_session_cost` advances the cap-basis but does
    // not check the warning threshold itself; if reviewer / classifier
    // spend pushes past `cost_warn_percent`, the `CostWarning` event
    // fires at the next `record_provider_cost` call (typically the next
    // main-turn round). This test pins that contract.
    let config = squeezy_core::AppConfig {
        max_session_cost_usd_micros: Some(10_000),
        cost_warn_percent: 80,
        ..Default::default()
    };
    let mut broker = CostBroker::new(&config);
    broker.seed_session(
        &squeezy_core::CostSnapshot::default(),
        squeezy_llm::TokenCalibration::default(),
    );
    // Out-of-band reviewer spend pushes the broker past the 80% warn
    // threshold (8_500 / 10_000 = 85%) but does NOT fire CostWarning.
    broker.record_out_of_band_session_cost(8_500);
    assert_eq!(broker.session_cost_usd_micros, 8_500);
    assert!(
        !broker.warn_emitted,
        "out-of-band record must not flip warn_emitted by itself"
    );
    // The next main-turn round picks up the lag and fires CostWarning
    // exactly once, even for a zero-cost provider round (which still
    // observes the threshold crossing on entry).
    let small_round = squeezy_core::CostSnapshot {
        estimated_usd_micros: Some(100),
        ..Default::default()
    };
    let status = broker.record_provider_cost(
        "anthropic",
        "claude-haiku-4-5",
        CostOrigin::Main,
        &small_round,
    );
    assert!(
        status.is_some(),
        "next main-turn round must fire CostWarning to surface the prior out-of-band threshold crossing"
    );
    assert!(broker.warn_emitted);
    // And the warning is one-shot — a subsequent priced round does not
    // re-fire it.
    let status_again = broker.record_provider_cost(
        "anthropic",
        "claude-haiku-4-5",
        CostOrigin::Main,
        &small_round,
    );
    assert!(
        status_again.is_none(),
        "CostWarning is one-shot per session even after the out-of-band fold"
    );
}

#[test]
fn session_metrics_without_model_ledger_field_deserializes_empty() {
    // A session persisted before `model_ledger` existed (field absent) must
    // deserialize with an empty ledger and never error — the resume-safety
    // contract for the additive `#[serde(default)]` field.
    let mut value = serde_json::to_value(squeezy_core::SessionMetrics::default())
        .expect("serialize default SessionMetrics");
    value
        .as_object_mut()
        .expect("metrics serialize to a JSON object")
        .remove("model_ledger");
    let metrics: squeezy_core::SessionMetrics =
        serde_json::from_value(value).expect("legacy SessionMetrics without model_ledger");
    assert!(metrics.model_ledger.is_empty());
}
