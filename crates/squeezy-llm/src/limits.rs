//! Layered, provenance-tracked resolution of a model's context window and
//! output limit.
//!
//! Historically the context window was resolved two different ways — the
//! `/context` display read `model_info_for().limits` (with an Ollama-only
//! override) while compaction re-derived it separately — and neither consulted
//! the cached models.dev catalog. This module is the single place that resolves
//! both, in a defined precedence, and records WHERE the number came from so the
//! UI can tell an exact user value from a 272K guess.
//!
//! Precedence (highest first):
//! 1. explicit per-model user override
//! 2. provider-live metadata (e.g. Ollama `/api/show`)
//! 3. curated bundled `models.json` entry
//! 4. cached models.dev catalog
//! 5. synthetic fallback
//!
//! On top of that, an *observed ceiling* (a provider context-overflow error
//! proving the real window is no larger than what overflowed) clamps the
//! selected window down regardless of which layer produced it — an over-
//! optimistic user value or stale catalog must not survive a hard rejection.

use crate::models_dev::{ModelsDevLimits, ModelsDevView};
use crate::registry::{
    DEFAULT_BASELINE_RESERVE_TOKENS, curated_model_info_for,
    default_effective_context_window_percent,
};

/// Where a resolved context-window value came from. Ordered loosely by trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitSource {
    /// Explicit per-model (or global) operator override.
    UserOverride,
    /// Live provider metadata probed this session (Ollama today).
    ProviderLive,
    /// Hand-curated bundled `models.json` entry (provenance URL in metadata).
    CuratedBundle,
    /// The locally cached models.dev catalog.
    ModelsDevCache,
    /// Clamped down by a provider context-window-exceeded error.
    ObservedBound,
    /// Conservative guess used when nothing else resolved.
    SyntheticFallback,
}

impl LimitSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserOverride => "user override",
            Self::ProviderLive => "provider live",
            Self::CuratedBundle => "curated bundle",
            Self::ModelsDevCache => "models.dev",
            Self::ObservedBound => "observed",
            Self::SyntheticFallback => "synthetic fallback",
        }
    }

    pub const fn confidence(self) -> LimitConfidence {
        match self {
            Self::UserOverride | Self::ProviderLive | Self::ObservedBound => LimitConfidence::Exact,
            Self::CuratedBundle => LimitConfidence::High,
            Self::ModelsDevCache => LimitConfidence::Medium,
            Self::SyntheticFallback => LimitConfidence::Low,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitConfidence {
    Exact,
    High,
    Medium,
    Low,
}

impl LimitConfidence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// Conservative window/output used when nothing else resolves. Mirrors the
/// historical `fallback_model_info` numbers so behaviour for unknown models is
/// unchanged.
pub const SYNTHETIC_FALLBACK_CONTEXT_WINDOW: u64 = 272_000;
pub const SYNTHETIC_FALLBACK_MAX_OUTPUT: u64 = 64_000;

/// Inputs to [`resolve_context_limits`]. Construct with [`ContextLimitInput::new`]
/// and set only the layers a caller actually has. `models_dev` is borrowed so
/// callers (and tests) control whether the cached catalog is consulted at all.
#[derive(Debug, Clone, Copy)]
pub struct ContextLimitInput<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub user_override: Option<u64>,
    pub provider_live_window: Option<u64>,
    pub observed_ceiling: Option<u64>,
    pub models_dev: Option<&'a ModelsDevView>,
    pub effective_percent_override: Option<u8>,
    pub baseline_reserve_override: Option<u64>,
}

impl<'a> ContextLimitInput<'a> {
    pub fn new(provider: &'a str, model: &'a str) -> Self {
        Self {
            provider,
            model,
            user_override: None,
            provider_live_window: None,
            observed_ceiling: None,
            models_dev: None,
            effective_percent_override: None,
            baseline_reserve_override: None,
        }
    }
}

/// The resolved limits plus provenance. `Copy` so it can ride inside
/// `RequestTokenEstimate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedContextLimits {
    pub context_window_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    /// Percent of the raw window treated as usable (the rest is headroom).
    pub effective_context_window_percent: u8,
    /// Flat token reserve carved off the effective window for system framing.
    pub baseline_reserve_tokens: u64,
    pub source: LimitSource,
    pub confidence: LimitConfidence,
    /// Set when an observed provider overflow clamped the window.
    pub observed_ceiling_tokens: Option<u64>,
    /// The window models.dev reports (even when not selected) so the UI can
    /// surface "models.dev says X" against a stale curated value.
    pub models_dev_window_tokens: Option<u64>,
}

/// Resolve a model's context window + output limit with provenance. Pure and
/// I/O-free: any network/disk work (Ollama probe, models.dev cache read) is the
/// caller's job, surfaced through [`ContextLimitInput`].
pub fn resolve_context_limits(input: &ContextLimitInput<'_>) -> ResolvedContextLimits {
    let curated = curated_model_info_for(input.provider, input.model);
    let curated_limits = curated.and_then(|info| info.limits);
    let models_dev_limits: Option<ModelsDevLimits> = input
        .models_dev
        .and_then(|view| view.lookup(input.provider, input.model));
    let models_dev_window_tokens = models_dev_limits.and_then(|limits| limits.context_window);

    // Base window + source, highest-trust layer first.
    let (mut window, mut source) = if let Some(window) = input.user_override {
        (Some(window), LimitSource::UserOverride)
    } else if let Some(window) = input.provider_live_window {
        (Some(window), LimitSource::ProviderLive)
    } else if let Some(window) = curated_limits.map(|limits| limits.context_window_tokens) {
        (Some(window), LimitSource::CuratedBundle)
    } else if let Some(window) = models_dev_window_tokens {
        (Some(window), LimitSource::ModelsDevCache)
    } else {
        (None, LimitSource::SyntheticFallback)
    };

    // An observed overflow is a hard fact: clamp below any layer, including an
    // over-optimistic user override or a stale catalog value.
    if let Some(ceiling) = input.observed_ceiling {
        match window {
            Some(current) if ceiling < current => {
                window = Some(ceiling);
                source = LimitSource::ObservedBound;
            }
            None => {
                window = Some(ceiling);
                source = LimitSource::ObservedBound;
            }
            _ => {}
        }
    }

    // Nothing produced a window — fall back to the conservative guess. `source`
    // is already `SyntheticFallback` in that branch.
    let context_window_tokens = Some(window.unwrap_or(SYNTHETIC_FALLBACK_CONTEXT_WINDOW));

    let max_output_tokens = curated_limits
        .map(|limits| limits.max_output_tokens)
        .or_else(|| models_dev_limits.and_then(|limits| limits.max_output))
        .or(Some(SYNTHETIC_FALLBACK_MAX_OUTPUT));

    // Clamp to 1..=100 so a bad override/catalog value can neither zero the
    // window (0) nor inflate it past the raw size (>100).
    let effective_context_window_percent = input
        .effective_percent_override
        .or_else(|| curated_limits.map(|limits| limits.effective_context_window_percent))
        .unwrap_or_else(default_effective_context_window_percent)
        .clamp(1, 100);

    let baseline_reserve_tokens = input
        .baseline_reserve_override
        .unwrap_or(DEFAULT_BASELINE_RESERVE_TOKENS);

    ResolvedContextLimits {
        context_window_tokens,
        max_output_tokens,
        effective_context_window_percent,
        baseline_reserve_tokens,
        source,
        confidence: source.confidence(),
        observed_ceiling_tokens: input.observed_ceiling,
        models_dev_window_tokens,
    }
}

/// Convenience: the effective (usable) window after applying the percent and
/// baseline reserve. Shared by the estimator and any caller that needs the
/// "safe" budget without rebuilding a full estimate.
pub fn effective_window_tokens(resolved: &ResolvedContextLimits) -> Option<u64> {
    resolved.context_window_tokens.map(|window| {
        window
            .saturating_mul(u64::from(resolved.effective_context_window_percent))
            .saturating_div(100)
            .saturating_sub(resolved.baseline_reserve_tokens)
    })
}

#[cfg(test)]
#[path = "limits_tests.rs"]
mod tests;
