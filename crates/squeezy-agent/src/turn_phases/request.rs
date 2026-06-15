//! Request assembly helpers for `TurnRuntime`.
//!
//! These helpers shape the LLM request envelope: response controls, reasoning
//! effort, beta headers, routed-tier effort, and per-round tool-choice policy.

use super::super::*;

pub(crate) fn request_response_verbosity(
    config: &AppConfig,
    provider_name: &str,
) -> Option<ResponseVerbosity> {
    capabilities_for(provider_name, &config.model)
        .filter(|capabilities| capabilities.text_verbosity)
        .map(|_| config.tui.response_verbosity)
}

pub(crate) fn request_reasoning_effort(
    config: &AppConfig,
    provider_name: &str,
) -> Option<squeezy_core::ReasoningEffort> {
    let effort = config.reasoning_effort?;
    capabilities_for(provider_name, &config.model)
        .filter(|capabilities| capabilities.reasoning_effort)
        .map(|_| effort)
}

/// Anthropic beta opt-ins to attach to a request, derived from the
/// `[model].context_1m` / `extended_thinking` flags. Returns an empty list
/// unless the active provider speaks the Anthropic Messages API (1P
/// `anthropic` or `bedrock`), so the betas never reach a provider that would
/// reject them. The transport layer joins them into the `anthropic-beta`
/// header (1P) or `additional_model_request_fields.anthropic_beta` (Bedrock).
pub(crate) fn request_beta_headers(config: &AppConfig, provider_name: &str) -> Arc<[Arc<str>]> {
    if !matches!(provider_name, "anthropic" | "bedrock") {
        return Arc::from(Vec::new());
    }
    let mut betas: Vec<Arc<str>> = Vec::new();
    if config.context_1m {
        betas.push(Arc::from(CONTEXT_1M_BETA));
    }
    if config.extended_thinking {
        betas.push(Arc::from(INTERLEAVED_THINKING_BETA));
    }
    Arc::from(betas)
}

/// Reasoning effort for the live routed rung. Effort is a cost lever orthogonal
/// to model choice, so a routed turn runs each rung at the effort it warrants
/// (weak→low … strong→high) instead of one global effort for every rung.
///
/// Precedence: an explicit user pin (`config.reasoning_effort`) always wins and
/// behaves exactly as [`request_reasoning_effort`]. Otherwise, when tier-effort
/// is enabled and routing is on: a per-task `judge_effort` (from the routing
/// judge, when `[routing].judge_effort` is on) overrides the static map, so two
/// turns on the same rung can run at different depths; failing that, the rung's
/// effort ([`RoutingConfig::effort_for_tier`] → [`ModelTier::default_effort`]).
/// With tier-effort off, or routing disabled, we fall back to the global path so
/// behavior is unchanged. The capability gate runs against the LIVE routed
/// `model` (not `config.model`), so a non-reasoning rung correctly drops the
/// field even when the parent supports effort.
pub(crate) fn request_reasoning_effort_for_tier(
    config: &AppConfig,
    provider_name: &str,
    model: &str,
    tier: ModelTier,
    judge_effort: Option<squeezy_core::ReasoningEffort>,
) -> Option<squeezy_core::ReasoningEffort> {
    let routing = &config.routing;
    // Tier-effort off (or routing off): the user pin applies as-is on every rung
    // (legacy behavior); no pin → provider default.
    if !routing.tier_effort || !routing.enabled {
        let effort = config.reasoning_effort?;
        return capabilities_for(provider_name, model)
            .filter(|capabilities| capabilities.reasoning_effort)
            .map(|_| effort);
    }
    let effort = if tier == ModelTier::Strong {
        // Strong/parent rung: the user pin governs how hard the main model
        // thinks; failing a pin, a per-task judge effort (Opus@xhigh for a tricky
        // bug vs Opus@medium for a routine edit) or the `effort_strong` override;
        // failing all, the provider default (None ends the turn here = no field).
        config
            .reasoning_effort
            .or(judge_effort)
            .or_else(|| routing.effort_for_tier(ModelTier::Strong))?
    } else {
        // Routed DOWN to a cheaper rung: run at the rung's shallow tier effort.
        // A user pin (or a judge `xhigh`) must NOT deepen a turn squeezy routed
        // down to economize — spending a 60k-token thinking budget on the cheap
        // model for a trivial turn would defeat the routing — so the pin can only
        // ever LOWER a cheap rung's effort, never raise it.
        let rung = routing
            .effort_for_tier(tier)
            .unwrap_or(tier.default_effort());
        match config.reasoning_effort {
            Some(pin) => rung.min(pin),
            None => rung,
        }
    };
    capabilities_for(provider_name, model)
        .filter(|capabilities| capabilities.reasoning_effort)
        .map(|_| effort)
}

/// Effective reasoning effort for a spawned subagent of `kind`.
///
/// Catalog roles override the parent's inherited global effort with their
/// own tuned default (Planner=High, Explorer/Reviewer=Low) so the priciest
/// reasoning tier is spent only where the plan justifies it; kinds without a
/// catalog role (Delegate, DocHelp) keep `inherited`. This only sets the
/// config field — provider/model capability is still gated downstream by
/// [`request_reasoning_effort`], so a non-reasoning provider drops the field
/// exactly as it would for the global path.
pub(crate) fn subagent_role_reasoning_effort(
    kind: SubagentKind,
    inherited: Option<squeezy_core::ReasoningEffort>,
) -> Option<squeezy_core::ReasoningEffort> {
    kind.role()
        .and_then(|role| role_config(role).reasoning_effort)
        .or(inherited)
}

/// Resolve the `tool_choice` to send on a given round of a turn.
///
/// `"required"` is configured to fix tool-shy models (Qwen via
/// OpenRouter, smaller MoEs) that emit a chatty preamble + finish
/// without calling any tool — but applying it on *every* round would
/// trap the model in an infinite call-tool-then-be-forced-to-call-tool
/// loop where it can never naturally end the turn with a text answer.
/// Downgrade to `"auto"` after round 0 so the model can finish once
/// it has the data it needs. Other configured values (`"auto"`,
/// `"none"`) pass through unchanged on every round; `None` keeps the
/// field absent.
pub(crate) fn effective_tool_choice(configured: Option<&str>, round: usize) -> Option<String> {
    match configured {
        Some("required") if round > 0 => Some("auto".to_string()),
        other => other.map(str::to_string),
    }
}
