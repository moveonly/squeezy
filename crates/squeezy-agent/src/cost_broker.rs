use std::io;

use serde::Serialize;
use serde_json::Value;
use squeezy_core::{AppConfig, CostOrigin, CostSnapshot, TurnMetrics};
use squeezy_llm::{LlmInputItem, LlmRequest, estimate_cost};
use squeezy_tools::{ToolResult, ToolStatus};

use crate::is_budget_denied;

/// Fallback projection used when the next-turn output token count is unknown
/// (e.g. the configured `max_output_tokens` is `None` and the model registry
/// has no curated `max_output_tokens` for this `(provider, model)` pair).
/// Picked to cover a "small but non-trivial" reply — keeps the pre-flight
/// estimate conservative on cheap models without being so high it rejects
/// every turn the moment a session warms up.
const PROJECTED_OUTPUT_TOKEN_FALLBACK: u64 = 1024;

/// Percent of the configured session cost cap at which the broker stops
/// starting *new* provider rounds (the adaptive pressure governor, gate
/// variant). Chosen below the hard cap so the session lands on a clean
/// "approaching your cap" boundary instead of overshooting it, and well
/// above the default `cost_warn_percent` heads-up so the gate fires after
/// the user has already been warned once. The gate only engages when a cap
/// is actually configured (`max_session_cost_usd_micros` is `Some`); with no
/// cap there is no pressure to govern and behaviour is unchanged.
const PRESSURE_GATE_PERCENT: u8 = 80;

/// Snapshot of session-level cost-cap state delivered with
/// [`AgentEvent::CostWarning`] and [`AgentEvent::Failed`] when the broker
/// crosses or reaches the configured cap. All values are USD micros (i.e.
/// 1 USD = 1_000_000 micros) so callers stay in integer math; `percent`
/// is `(spent / cap) * 100` clamped at 255 to avoid overflow on extreme
/// overshoot.
#[derive(Debug, Clone, Copy)]
pub struct CostCapStatus {
    pub spent_usd_micros: u64,
    pub cap_usd_micros: u64,
    pub percent: u8,
}

/// Pre-flight snapshot of a single round's projected input size against the
/// configured `max_round_input_tokens` ceiling. Mirrors [`CostCapStatus`]'s
/// style so the agent can render a clear gate notice in the same shape as the
/// session-cost cap. `estimated_input_tokens` is the `estimate_context`
/// projection for the assembled request; `limit_tokens` is the configured
/// ceiling; `estimated_usd_micros` is the registry-priced dollar value of the
/// projected round (`None` when the active `(provider, model)` has no pricing).
#[derive(Debug, Clone, Copy)]
pub struct RoundInputGateStatus {
    pub estimated_input_tokens: u64,
    pub limit_tokens: u64,
    pub estimated_usd_micros: Option<u64>,
}

/// Pre-flight gate decision. Returns `Some(status)` when a round whose
/// assembled request is estimated at `estimated_input_tokens` would exceed the
/// configured `max_round_input_tokens` ceiling, so the caller can compact (or
/// gate) *before* paying for the oversized round. Returns `None` when the gate
/// is unset (`None`) or the estimate is at/under the ceiling — i.e. the default
/// path is a single `Option` check with no behaviour change.
///
/// The dollar figure is computed with the same `estimate_cost` + registry
/// pricing the session-cost cap uses, so the gate notice can quote "$X for this
/// round" without a second cost model. It is best-effort: an unpriced model
/// yields `None` for the dollar field but the token gate still fires.
pub(crate) fn round_input_gate_status(
    max_round_input_tokens: Option<u64>,
    estimated_input_tokens: u64,
    provider: &str,
    model: &str,
    projected_output_tokens: u64,
) -> Option<RoundInputGateStatus> {
    let limit_tokens = max_round_input_tokens?;
    if estimated_input_tokens <= limit_tokens {
        return None;
    }
    let projection = CostSnapshot {
        input_tokens: Some(estimated_input_tokens),
        output_tokens: Some(projected_output_tokens),
        ..Default::default()
    };
    Some(RoundInputGateStatus {
        estimated_input_tokens,
        limit_tokens,
        estimated_usd_micros: estimate_cost(provider, model, &projection),
    })
}

/// Per-turn running cost+tool-count snapshot emitted via
/// `AgentEvent::CostUpdate` so a user watching a live transcript can see
/// expense accumulating before the turn footer arrives.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CostProgressSnapshot {
    pub(crate) tool_count: u64,
    pub(crate) input_tokens: u64,
    pub(crate) micro_usd: u64,
}

#[derive(Debug)]
pub(crate) struct CostBroker {
    max_tool_calls: u64,
    max_bytes_read: u64,
    max_search_files: u64,
    pub(crate) metrics: TurnMetrics,
    /// Cumulative observed provider cost for the entire session, in USD
    /// micros. Seeded from the resumed conversation state and updated after
    /// every provider response we record.
    session_cost_usd_micros: u64,
    /// Cumulative session cost as a full [`CostSnapshot`] (token distribution and
    /// USD), seeded from the resumed session and advanced on every recorded round.
    ///
    /// Mirrors `session_cost_usd_micros` on the dollar field but also carries
    /// input/output/cache tokens, so the live status line can show a
    /// session-cumulative cost+token snapshot without re-reading conversation
    /// state. Does not include out-of-band reviewer spend recorded straight to
    /// the session (a small, next-turn-reconciled lag).
    session_cost: CostSnapshot,
    /// Hard cap from `AppConfig.max_session_cost_usd_micros`. `None` (or a
    /// zero cap) disables session-level gating.
    max_session_cost_usd_micros: Option<u64>,
    /// Percent of the cap at which the broker emits a single
    /// `CostCapStatus` warning. Mirrors `AppConfig.cost_warn_percent`.
    cost_warn_percent: u8,
    /// One-shot latch so the warning event is emitted at most once per
    /// broker (and therefore at most once per session: the main turn
    /// broker is rebuilt with the cumulative session total each turn,
    /// but `warn_emitted` follows from `session_cost_usd_micros` already
    /// being above the threshold at construction).
    warn_emitted: bool,
    /// One-shot latch for the "cap configured but pricing unknown" notice.
    /// Set the first time a round is recorded under a configured cap with
    /// no per-round dollar estimate, so the user is told once that the cap
    /// cannot be enforced for this `(provider, model)` instead of the cap
    /// silently no-op'ing.
    cap_unenforceable_emitted: bool,
    /// One-shot latch for the adaptive pressure governor (gate variant). Set
    /// the first time [`CostBroker::pressure_gate`] refuses to start a new
    /// provider round because spend has reached `PRESSURE_GATE_PERCENT` of the
    /// cap, so the gate fires at most once per broker rather than re-tripping
    /// on every subsequent round once over the threshold.
    pressure_gate_emitted: bool,
    /// Per-token-byte calibration carried through the turn. Seeded from
    /// the session metadata (or the global file) and snapshot back out
    /// after every recorded provider response.
    pub(crate) calibration: squeezy_llm::TokenCalibration,
}

impl CostBroker {
    pub(crate) fn new(config: &AppConfig) -> Self {
        Self {
            max_tool_calls: config.max_tool_calls_per_turn,
            max_bytes_read: config.max_tool_bytes_read_per_turn,
            max_search_files: config.max_search_files_per_turn,
            metrics: TurnMetrics::default(),
            session_cost_usd_micros: 0,
            session_cost: CostSnapshot::default(),
            max_session_cost_usd_micros: config.max_session_cost_usd_micros.filter(|cap| *cap > 0),
            cost_warn_percent: config.cost_warn_percent.clamp(1, 100),
            warn_emitted: false,
            cap_unenforceable_emitted: false,
            pressure_gate_emitted: false,
            calibration: squeezy_llm::TokenCalibration::default(),
        }
    }

    /// Seed the running session cost from a resumed `CostSnapshot`. Pre-seeds
    /// `warn_emitted` so a session that resumes already over the warning
    /// threshold doesn't re-fire the warning on its first new turn. Stores the
    /// full prior snapshot so the cumulative `session_cost` (tokens + USD) is
    /// correct from the first new round.
    pub(crate) fn seed_session(
        &mut self,
        prior_cost: &CostSnapshot,
        calibration: squeezy_llm::TokenCalibration,
    ) {
        self.session_cost = prior_cost.clone();
        self.session_cost_usd_micros = prior_cost.estimated_usd_micros.unwrap_or(0);
        self.calibration = calibration;
        if let Some(cap) = self.max_session_cost_usd_micros {
            let threshold = warn_threshold_micros(cap, self.cost_warn_percent);
            if self.session_cost_usd_micros >= threshold {
                self.warn_emitted = true;
            }
        }
    }

    /// Records the provider-reported cost from a single LLM round. Adds
    /// `estimated_usd_micros` to the running session total and returns
    /// `Some(CostCapStatus)` the first time the session crosses
    /// `cost_warn_percent` (or hits the cap), so the caller can publish a
    /// transcript event.
    ///
    /// `provider`/`model`/`origin` attribute the round to its `(provider,
    /// model)` bucket in the per-model ledger. The ledger is additive-only and
    /// parallel to the flat `metrics.provider` total — it is never summed back
    /// into `session_cost_usd_micros`, so the dollar total can't drift.
    pub(crate) fn record_provider_cost(
        &mut self,
        provider: &str,
        model: &str,
        origin: CostOrigin,
        cost: &CostSnapshot,
    ) -> Option<CostCapStatus> {
        self.metrics.record_provider(cost);
        self.metrics
            .model_ledger
            .record(provider, model, origin, cost);
        crate::merge_cost(&mut self.session_cost, cost);
        let delta = cost.estimated_usd_micros.unwrap_or(0);
        self.session_cost_usd_micros = self.session_cost_usd_micros.saturating_add(delta);
        let cap = self.max_session_cost_usd_micros?;
        if self.warn_emitted {
            return None;
        }
        let threshold = warn_threshold_micros(cap, self.cost_warn_percent);
        if self.session_cost_usd_micros < threshold {
            return None;
        }
        self.warn_emitted = true;
        Some(CostCapStatus {
            spent_usd_micros: self.session_cost_usd_micros,
            cap_usd_micros: cap,
            percent: cap_percent(self.session_cost_usd_micros, cap),
        })
    }

    /// The session-cumulative cost snapshot (token distribution + USD): seeded
    /// from the resumed session and advanced by every `record_provider_cost`.
    /// The dollar field is canonicalised to `session_cost_usd_micros` (the
    /// authoritative cap-basis total) so the live status line always shows the
    /// same figure the cap enforces. Emitted on cost-bearing agent events so
    /// the status line shows session-cumulative spend that survives a mid-turn
    /// cancel or failure.
    pub(crate) fn session_cost_snapshot(&self) -> CostSnapshot {
        CostSnapshot {
            estimated_usd_micros: Some(self.session_cost_usd_micros),
            ..self.session_cost.clone()
        }
    }

    /// Reports whether a configured session cost cap cannot be enforced for
    /// the round just recorded, returning `true` exactly once.
    ///
    /// The cap is dollar-based, but a round whose `(provider, model)` has no
    /// registry pricing yields `estimated_usd_micros == None`: the running
    /// total can't advance, so neither the warning threshold nor the hard cap
    /// ever fires. Left silent, a guardrail the user explicitly configured is
    /// a no-op with no feedback. The one-shot latch lets the caller surface a
    /// single transcript notice that the cap is inert for this model rather
    /// than failing closed on an unpriced round.
    pub(crate) fn note_unenforceable_cap_round(&mut self, cost: &CostSnapshot) -> bool {
        if self.max_session_cost_usd_micros.is_none()
            || cost.estimated_usd_micros.is_some()
            || self.cap_unenforceable_emitted
        {
            return false;
        }
        self.cap_unenforceable_emitted = true;
        true
    }

    /// Returns `Some(status)` if the running session cost has reached or
    /// exceeded the configured cap. Used to refuse the next provider round.
    pub(crate) fn session_cap_reached(&self) -> Option<CostCapStatus> {
        let cap = self.max_session_cost_usd_micros?;
        if self.session_cost_usd_micros >= cap {
            Some(CostCapStatus {
                spent_usd_micros: self.session_cost_usd_micros,
                cap_usd_micros: cap,
                percent: cap_percent(self.session_cost_usd_micros, cap),
            })
        } else {
            None
        }
    }

    /// Current session spend as a percent of the configured cost cap, or
    /// `None` when no cap is set (there is nothing to be a percent *of*).
    /// Capped at 255 to match [`cap_percent`] so an overshoot can't overflow
    /// the `u8`. This is the raw pressure signal the gate is built on; exposed
    /// separately so callers (and tests) can read the headroom without
    /// triggering the one-shot gate latch.
    pub(crate) fn pressure_percent(&self) -> Option<u8> {
        let cap = self.max_session_cost_usd_micros?;
        Some(cap_percent(self.session_cost_usd_micros, cap))
    }

    /// Adaptive pressure governor (gate variant): returns `Some(status)` the
    /// first time the running session spend has reached `PRESSURE_GATE_PERCENT`
    /// of the configured cap, signalling the caller to *refuse to start the
    /// next provider round* rather than silently shrinking per-turn budgets.
    ///
    /// This is the deliberately low-risk shape of B6: no per-turn budget
    /// mutation, no silent capability degradation — just a clean stop at a
    /// named pressure boundary below the hard cap, so the session ends on
    /// "you're approaching your cap" instead of overshooting it. The latch
    /// makes it fire at most once per broker; combined with the post-hoc
    /// `session_cap_reached` and pre-flight `projected_session_cap_overrun`
    /// hard-cap checks, the gate adds an early, advisory-strength stop.
    ///
    /// Returns `None` when:
    ///   - no cap is configured (no pressure to govern — behaviour unchanged),
    ///   - spend is still below `PRESSURE_GATE_PERCENT` of the cap,
    ///   - the gate has already fired once for this broker.
    pub(crate) fn pressure_gate(&mut self) -> Option<CostCapStatus> {
        if self.pressure_gate_emitted {
            return None;
        }
        // `pressure_percent` is `None` exactly when no cap is configured, so a
        // missing percent leaves the gate open and behaviour unchanged.
        let percent = self.pressure_percent()?;
        if percent < PRESSURE_GATE_PERCENT {
            return None;
        }
        let cap = self.max_session_cost_usd_micros?;
        self.pressure_gate_emitted = true;
        Some(CostCapStatus {
            spent_usd_micros: self.session_cost_usd_micros,
            cap_usd_micros: cap,
            percent,
        })
    }

    /// Pre-flight check: returns `Some(status)` if dispatching another LLM
    /// round at the supplied `(provider, model)` with `projected_input_tokens`
    /// on the wire and `projected_output_tokens` of model reply would push the
    /// running session total at or past the configured cap.
    ///
    /// `projected_output_tokens` should be the caller's best estimate of the
    /// next reply size — typically `AppConfig.max_output_tokens` if set,
    /// otherwise the model-registry curated `max_output_tokens`, otherwise
    /// `PROJECTED_OUTPUT_TOKEN_FALLBACK` for unknown providers. We deliberately
    /// project against the worst case (the model could use every token of its
    /// max output budget) so the cap stops the dispatch *before* the over-cap
    /// spend is billed.
    ///
    /// Returns `None` when:
    ///   - no cap is configured,
    ///   - the model registry has no pricing for `(provider, model)` (we can't
    ///     project without a per-Mtok price, so we fall through to the
    ///     post-hoc check),
    ///   - the projected total is still under the cap.
    ///
    /// The returned `spent_usd_micros` is the *projected* total (current
    /// spend + estimate), so the failure message reflects "we would have
    /// landed here" rather than the misleading "we already landed here".
    pub(crate) fn projected_session_cap_overrun(
        &self,
        provider: &str,
        model: &str,
        projected_input_tokens: u64,
        projected_output_tokens: u64,
    ) -> Option<CostCapStatus> {
        let cap = self.max_session_cost_usd_micros?;
        let projection = CostSnapshot {
            input_tokens: Some(projected_input_tokens),
            output_tokens: Some(projected_output_tokens),
            ..Default::default()
        };
        let projected_round_micros = estimate_cost(provider, model, &projection)?;
        let projected_total = self
            .session_cost_usd_micros
            .saturating_add(projected_round_micros);
        if projected_total < cap {
            return None;
        }
        Some(CostCapStatus {
            spent_usd_micros: projected_total,
            cap_usd_micros: cap,
            percent: cap_percent(projected_total, cap),
        })
    }

    /// Conservative output-token estimate for the upcoming round. Used by the
    /// agent's pre-flight cap check; centralised here so the broker owns the
    /// "what's a sensible fallback?" policy.
    pub(crate) fn projected_output_tokens(
        configured_max_output_tokens: Option<u32>,
        model_max_output_tokens: Option<u64>,
    ) -> u64 {
        configured_max_output_tokens
            .map(u64::from)
            .or(model_max_output_tokens)
            .unwrap_or(PROJECTED_OUTPUT_TOKEN_FALLBACK)
    }

    pub(crate) fn reserve_call(&mut self) -> Result<u64, (u64, String)> {
        self.metrics.tool_calls += 1;
        let tool_sequence = self.metrics.tool_calls;
        if tool_sequence > self.max_tool_calls {
            Err((
                tool_sequence,
                format!(
                    "per-turn tool-call budget exceeded: limit={}",
                    self.max_tool_calls
                ),
            ))
        } else {
            Ok(tool_sequence)
        }
    }

    pub(crate) fn deny_reason(&self) -> Option<String> {
        if self.metrics.bytes_read >= self.max_bytes_read {
            Some(format!(
                "per-turn tool byte-read budget exceeded: limit={}",
                self.max_bytes_read
            ))
        } else if self.metrics.files_scanned >= self.max_search_files {
            Some(format!(
                "per-turn search file-scan budget exceeded: limit={}",
                self.max_search_files
            ))
        } else {
            None
        }
    }

    pub(crate) fn enforces_result_budgets(&self) -> bool {
        self.max_bytes_read < u64::MAX || self.max_search_files < u64::MAX
    }

    pub(crate) fn record_executed_result(&mut self, result: &ToolResult) {
        match result.status {
            ToolStatus::Success => self.metrics.tool_successes += 1,
            ToolStatus::Error | ToolStatus::Stale => self.metrics.tool_errors += 1,
            ToolStatus::Denied => self.metrics.tool_denials += 1,
            ToolStatus::Cancelled => self.metrics.tool_cancellations += 1,
        }
        self.metrics.files_scanned += result.cost_hint.files_scanned;
        self.metrics.bytes_read += result.cost_hint.bytes_read;
        self.metrics.matches_returned += result.cost_hint.matches_returned;
        self.metrics.redactions += result.cost_hint.redactions;
        if result.content.get("spilled").and_then(Value::as_bool) == Some(true) {
            self.metrics.spill_writes += 1;
        }
        if result.tool_name == "read_tool_output" && result.status == ToolStatus::Success {
            self.metrics.spill_reads += 1;
        }
        if is_budget_denied(result) {
            self.metrics.budget_denials += 1;
        }
    }

    /// Snapshot the running per-turn progress when the executed-tool count
    /// is at a stride multiple, so callers can emit a single
    /// `AgentEvent::CostUpdate`. Returning `None` keeps the per-tool
    /// hot-path cheap and prevents firing on every call.
    pub(crate) fn progress_snapshot_if_due(&self, stride: u64) -> Option<CostProgressSnapshot> {
        let total = self
            .metrics
            .tool_successes
            .saturating_add(self.metrics.tool_errors)
            .saturating_add(self.metrics.tool_denials)
            .saturating_add(self.metrics.tool_cancellations);
        if stride == 0 || total == 0 || !total.is_multiple_of(stride) {
            return None;
        }
        Some(CostProgressSnapshot {
            tool_count: total,
            input_tokens: self.metrics.provider.input_tokens.unwrap_or(0),
            micro_usd: self.metrics.provider.estimated_usd_micros.unwrap_or(0),
        })
    }

    pub(crate) fn record_model_result(&mut self, result: &ToolResult) {
        self.metrics.model_output_bytes += result.model_output().len() as u64;
        if result.content.get("receipt_stub").and_then(Value::as_bool) == Some(true) {
            self.metrics.receipt_stub_hits += 1;
        }
        if result
            .content
            .get("negative_receipt_stub")
            .and_then(Value::as_bool)
            == Some(true)
        {
            self.metrics.negative_receipt_hits += 1;
        }
    }
}

/// Approximate the byte size of an LLM request's input payload. Used to feed
/// the token-calibration EMA: we cannot count provider tokens locally, but
/// we can pair the bytes we sent with the input-token count the provider
/// reports back. Counts instructions, every input item's text, and the
/// serialized tool spec list so the ratio reflects everything we actually
/// transmitted.
pub(crate) fn llm_request_input_bytes(request: &LlmRequest) -> u64 {
    let mut total: u64 = request.instructions.len() as u64;
    for item in request.input.iter() {
        total = total.saturating_add(match item {
            LlmInputItem::UserText(text) | LlmInputItem::AssistantText(text) => text.len() as u64,
            LlmInputItem::FunctionCallOutput { output, .. } => output.len() as u64,
            LlmInputItem::Image { bytes, .. } => bytes.len() as u64,
            // Document payloads (PDF/CSV/…) are transmitted on the wire
            // (Bedrock/Anthropic document blocks), so count their bytes for the
            // bytes→tokens calibration EMA — matching context_compaction's
            // accounting. Omitting them biased the ratio for document turns.
            LlmInputItem::Document { bytes, .. } => bytes.len() as u64,
            LlmInputItem::FunctionCall { arguments, .. } => serialized_json_len(arguments),
            LlmInputItem::Reasoning(payload) => payload.display_text().len() as u64,
            // `LlmInputItem` is `#[non_exhaustive]`; unknown future variants
            // contribute zero bytes to the calibration EMA until a
            // dedicated arm exists.
            _ => 0,
        });
    }
    for spec in request.tools.iter() {
        total = total.saturating_add(serialized_json_len(spec));
    }
    total
}

/// Byte size of the request's *fixed overhead* — the system instructions and
/// serialized tool schemas that ride along on every request but are NOT part
/// of the conversation items. `estimate_context` only walks conversation
/// items, so the post-turn compaction gate adds this overhead to avoid
/// under-counting the real request size (finding #2).
pub(crate) fn llm_request_overhead_bytes(request: &LlmRequest) -> u64 {
    let mut total: u64 = request.instructions.len() as u64;
    for spec in request.tools.iter() {
        total = total.saturating_add(serialized_json_len(spec));
    }
    total
}

#[derive(Debug, Default)]
struct JsonByteCounter {
    bytes: u64,
}

impl io::Write for JsonByteCounter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buf.len() as u64);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_json_len<T: Serialize>(value: &T) -> u64 {
    let mut counter = JsonByteCounter::default();
    serde_json::to_writer(&mut counter, value)
        .map(|()| counter.bytes)
        .unwrap_or(0)
}

/// Render the pre-flight round-input gate notice. Fired when an assembled
/// request's estimated input tokens exceed `max_round_input_tokens` even after
/// the mid-turn compaction attempt. Mirrors `format_cap_reached_reason`'s
/// shape: states the overage, quotes the registry-priced round cost when
/// available, and points at the knob to raise.
pub(crate) fn format_round_input_gate_reason(status: RoundInputGateStatus) -> String {
    let cost = match status.estimated_usd_micros {
        Some(micros) => format!(" (~${:.4} this round)", micros as f64 / 1_000_000.0),
        None => String::new(),
    };
    format!(
        "pre-flight round-input gate: estimated {} input tokens exceeds the \
         max_round_input_tokens ceiling of {}{}, and mid-turn compaction could \
         not bring it under. Run /config to raise `max_round_input_tokens` \
         (or set SQUEEZY_MAX_ROUND_INPUT_TOKENS), or /compact and retry.",
        status.estimated_input_tokens, status.limit_tokens, cost,
    )
}

pub(crate) fn format_cap_reached_reason(status: CostCapStatus) -> String {
    format!(
        "session cost cap reached: spent ${:.6} of ${:.6} ({}%). \
         Run /config to raise `max_session_cost_usd_micros` \
         (or set SQUEEZY_MAX_SESSION_COST_USD_MICROS), then send the next prompt.",
        status.spent_usd_micros as f64 / 1_000_000.0,
        status.cap_usd_micros as f64 / 1_000_000.0,
        status.percent,
    )
}

/// Render the adaptive-pressure-governor gate reason: the session reached the
/// pressure threshold below the hard cap, so the broker stopped before
/// starting another provider round. Carries the same next-step guidance as the
/// cap-reached error (raise the cap to continue) but frames it as a proactive
/// stop "approaching your cap" rather than a hard overrun.
pub fn format_pressure_gate_reason(status: CostCapStatus) -> String {
    format!(
        "session cost approaching cap: spent ${:.6} of ${:.6} ({}%); \
         paused before starting another round to avoid overshooting the cap. \
         Run /config to raise `max_session_cost_usd_micros` \
         (or set SQUEEZY_MAX_SESSION_COST_USD_MICROS), then send the next prompt.",
        status.spent_usd_micros as f64 / 1_000_000.0,
        status.cap_usd_micros as f64 / 1_000_000.0,
        status.percent,
    )
}

/// Render the cost-cap *warning* threshold notice with the same next-step
/// guidance as the cap-reached error. Surfaced by the TUI when the broker
/// reports a warning-tier `CostCapStatus` so the user can react before the
/// hard cap actually trips.
pub fn format_warn_threshold_notice(status: CostCapStatus) -> String {
    format!(
        "session cost crossed warning threshold: spent ${:.4} of ${:.2} cap ({}%). \
         Run /config to raise `max_session_cost_usd_micros` before the cap trips \
         (or set SQUEEZY_MAX_SESSION_COST_USD_MICROS).",
        status.spent_usd_micros as f64 / 1_000_000.0,
        status.cap_usd_micros as f64 / 1_000_000.0,
        status.percent,
    )
}

/// Render the one-time notice shown when a session cost cap is configured
/// but the active `(provider, model)` has no registry pricing, so the cap
/// cannot be enforced. Surfaced by the TUI when the broker reports an
/// unenforceable-cap round so the user knows the guardrail is inert instead
/// of silently trusting a cap that never trips.
pub fn format_cap_unenforceable_notice(provider: &str, model: &str) -> String {
    format!(
        "session cost cap configured but pricing for `{provider}/{model}` is unknown; \
         the cap cannot be enforced for this model. Switch to a model with known pricing, \
         or remove `max_session_cost_usd_micros` to silence this notice."
    )
}

fn warn_threshold_micros(cap_usd_micros: u64, warn_percent: u8) -> u64 {
    let percent = warn_percent.clamp(1, 100) as u128;
    (cap_usd_micros as u128 * percent / 100).min(u64::MAX as u128) as u64
}

fn cap_percent(spent_usd_micros: u64, cap_usd_micros: u64) -> u8 {
    if cap_usd_micros == 0 {
        return 0;
    }
    let percent = (spent_usd_micros as u128 * 100) / cap_usd_micros as u128;
    percent.min(255) as u8
}

#[cfg(test)]
#[path = "cost_broker_tests.rs"]
mod tests;
