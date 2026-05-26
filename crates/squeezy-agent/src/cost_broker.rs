use serde_json::Value;
use squeezy_core::{AppConfig, CostSnapshot, TurnMetrics};
use squeezy_llm::{LlmInputItem, LlmRequest};
use squeezy_tools::{ToolResult, ToolStatus};

use crate::is_budget_denied;

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
            max_session_cost_usd_micros: config.max_session_cost_usd_micros.filter(|cap| *cap > 0),
            cost_warn_percent: config.cost_warn_percent.clamp(1, 100),
            warn_emitted: false,
            calibration: squeezy_llm::TokenCalibration::default(),
        }
    }

    /// Seed the running session cost from a resumed `CostSnapshot`. Pre-seeds
    /// `warn_emitted` so a session that resumes already over the warning
    /// threshold doesn't re-fire the warning on its first new turn.
    pub(crate) fn seed_session(
        &mut self,
        session_cost_usd_micros: u64,
        calibration: squeezy_llm::TokenCalibration,
    ) {
        self.session_cost_usd_micros = session_cost_usd_micros;
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
    pub(crate) fn record_provider_cost(&mut self, cost: &CostSnapshot) -> Option<CostCapStatus> {
        self.metrics.record_provider(cost);
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
        if is_budget_denied(result) {
            self.metrics.budget_denials += 1;
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
            LlmInputItem::FunctionCall { arguments, .. } => serde_json::to_vec(arguments)
                .map(|v| v.len() as u64)
                .unwrap_or(0),
        });
    }
    for spec in request.tools.iter() {
        total = total.saturating_add(
            serde_json::to_vec(spec)
                .map(|v| v.len() as u64)
                .unwrap_or(0),
        );
    }
    total
}

pub(crate) fn format_cap_reached_reason(status: CostCapStatus) -> String {
    format!(
        "session cost cap reached: spent ${:.6} of ${:.6} ({}%)",
        status.spent_usd_micros as f64 / 1_000_000.0,
        status.cap_usd_micros as f64 / 1_000_000.0,
        status.percent,
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
