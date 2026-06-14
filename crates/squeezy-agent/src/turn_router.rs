//! Per-turn model routing — the "cheap-model fast path".
//!
//! A turn that starts on the user's headline model (Opus, Sonnet,
//! GPT-5.5, …) is silently dispatched to the provider's small-fast tier
//! (Haiku, GPT-mini, Gemini Flash Lite, Bedrock cheap variant, …) when
//! the user prompt is "obviously simple" — a single well-specified
//! operation such as "checkout the foo branch and run cargo test".
//!
//! Two-layer classifier:
//!   1. **Heuristic prefilter** — pure-Rust pattern match on imperative
//!      verbs, prompt length, multi-paragraph / ambiguity smells.
//!   2. **Cheap-tier LLM judge** — single short JSON-constrained
//!      classification call dispatched to the same cheap model that
//!      would run the routed turn. Only fires when the heuristic
//!      abstains and the prompt is shorter than `judge_max_chars`.
//!
//! Fallback is handled by [`EscalationState`], which the agent's
//! streaming loop polls after every tool result and assistant-text
//! delta. On signal (tool-call ceiling, error threshold, refusal
//! phrase, parse error) the agent calls `replace_provider` on its own
//! provider with the parent model and continues the same turn — no
//! replay required.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use squeezy_core::{
    AppConfig, CacheIsolation, CostSnapshot, ModelTier, RoutingConfig, SessionMode, TierLadder,
};
use squeezy_llm::{
    CacheRetention, CacheSpec, LlmEvent, LlmInputItem, LlmOutputSchema, LlmProvider, LlmRequest,
    provider_honors_output_schema,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CheapReason {
    HeuristicSlamDunk(&'static str),
    /// Matched an entry from the user's `[routing].extra_heuristic_verbs`
    /// allowlist rather than the built-in whitelist. Telemetry and the
    /// `AgentEvent::TurnRouted` reason string carry the literal verb
    /// (prefixed `extra_verb:`) so operators can audit which extension
    /// fires how often.
    ExtraVerb(Arc<str>),
    LlmJudge,
    UserExplicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscalationReason {
    ToolCallCeiling,
    ErrorThreshold,
    RefusalPhrase,
    ProviderError,
    ToolDiversity,
}

impl EscalationReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ToolCallCeiling => "tool_call_ceiling",
            Self::ErrorThreshold => "error_threshold",
            Self::RefusalPhrase => "refusal_phrase",
            Self::ProviderError => "provider_error",
            Self::ToolDiversity => "tool_diversity",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum TurnRoutingDecision {
    Parent,
    Cheap {
        reason: CheapReason,
        /// Which rung of the ladder this turn starts on — `Weak` or `Medium`
        /// (a `Strong` start is represented as `Parent`).
        tier: ModelTier,
        model: Arc<str>,
    },
}

impl TurnRoutingDecision {
    pub(crate) fn is_cheap(&self) -> bool {
        matches!(self, Self::Cheap { .. })
    }

    /// The starting tier for this turn. `Parent` is the strong (flagship) rung.
    pub(crate) fn tier(&self) -> ModelTier {
        match self {
            Self::Cheap { tier, .. } => *tier,
            Self::Parent => ModelTier::Strong,
        }
    }

    pub(crate) fn reason_label(&self) -> Option<String> {
        match self {
            Self::Cheap { reason, .. } => Some(match reason {
                CheapReason::HeuristicSlamDunk(rule) => (*rule).to_string(),
                CheapReason::ExtraVerb(verb) => format!("extra_verb:{verb}"),
                CheapReason::LlmJudge => "llm_judge".to_string(),
                CheapReason::UserExplicit => "user_explicit".to_string(),
            }),
            Self::Parent => None,
        }
    }
}

/// One-turn user overrides set by the `/cheap`, `/parent`, and `/router`
/// slash commands. The dispatcher reads these once per turn; the
/// transient `force_*` flags are cleared after consumption while
/// `session_disabled` persists for the rest of the session.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct RoutingOverride {
    pub force_cheap: bool,
    pub force_parent: bool,
    pub session_disabled: bool,
}

/// Sticky-window state. After an escalation, the next
/// `escalation_sticky_turns` user prompts are dispatched on the parent
/// model even if the classifier would route cheap — avoids flapping in
/// the middle of a hard task. Decremented at the top of each
/// `classify_turn` call.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct StickyEscalation {
    pub remaining_turns: u8,
}

impl StickyEscalation {
    pub fn engage(&mut self, sticky_turns: u8) {
        self.remaining_turns = self.remaining_turns.max(sticky_turns);
    }

    /// Returns `true` if the current turn must use the parent model
    /// because a recent escalation is still in its sticky window;
    /// decrements the counter as a side effect so the window expires
    /// naturally as the user continues to send prompts.
    pub fn tick(&mut self) -> bool {
        if self.remaining_turns == 0 {
            return false;
        }
        self.remaining_turns -= 1;
        true
    }
}

/// Cross-turn router state owned by `Agent`. Reset between turns where
/// appropriate (`pending_override.force_*` is one-shot); the sticky
/// window persists until it expires naturally.
#[derive(Debug, Default)]
pub(crate) struct RoutingPersistentState {
    pub sticky: StickyEscalation,
    pub pending_override: RoutingOverride,
}

const HEURISTIC_VERBS: &[&str] = &[
    "checkout", "rename", "run", "ls", "cat", "grep", "format", "fmt", "lint", "fetch", "stash",
    "tag",
];

/// Substrings that unambiguously flag Linux sandbox-sensitive territory.
/// These are either multi-word phrases, path prefixes, or hyphenated
/// commands that cannot appear as part of unrelated words.
const LINUX_SANDBOX_SENSITIVE_SUBSTRINGS: &[&str] = &[
    "/proc",
    "/sys",
    "apt-get",
    "network namespace",
    "kernel policy",
    "user namespace",
    "pivot_root",
    "overlayfs",
];

/// Single words that flag Linux sandbox-sensitive territory. Checked with
/// whole-word matching so short names like "apt" do not false-positive on
/// unrelated words (e.g. a hypothetical "captain").
const LINUX_SANDBOX_SENSITIVE_WORDS: &[&str] = &[
    "unshare",
    "landlock",
    "seccomp",
    "sudo",
    "systemd",
    "docker",
    "podman",
    "pacman",
    "zypper",
    "netns",
    "cgroup",
    "nsenter",
    "chroot",
    "containerd",
    "rootless",
    "userns",
    "apt",
    "yum",
    "dnf",
    "apk",
];

/// Returns `true` when `user_input` (case-insensitive) contains any
/// Linux sandbox-sensitive keyword. Used by `classify_turn` to prevent
/// the heuristic or judge from routing host-sensitive Linux work to
/// the cheap model tier.
///
/// Prompts involving Docker, Podman, container runtimes, and package
/// managers are treated as sandbox-sensitive even on macOS and Windows
/// because those workflows require the parent model's care on any platform.
pub(crate) fn is_linux_sandbox_sensitive(user_input: &str) -> bool {
    let lower = user_input.to_ascii_lowercase();
    if LINUX_SANDBOX_SENSITIVE_SUBSTRINGS
        .iter()
        .any(|term| lower.contains(term))
    {
        return true;
    }
    // Word-boundary check: split on non-alphanumeric, non-hyphen, non-underscore
    // characters so "apt" matches "run apt install" but not "captain".
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
        .filter(|w| !w.is_empty())
        .collect();
    LINUX_SANDBOX_SENSITIVE_WORDS
        .iter()
        .any(|term| words.contains(term))
}

const AMBIGUITY_MARKERS: &[&str] = &[
    "maybe",
    "figure out",
    "decide",
    "design",
    "refactor across",
    "should i",
    "should we",
    "what if",
    "think about",
    "let me know",
    "investigate",
    "explore",
    "research",
    "legacy",
    "across the",
    "any test",
    "any tests",
    "any file",
    "any files",
    "the one that",
    "carefully",
    "thoroughly",
    "safely",
    "without breaking",
    "be careful",
    "make sure not",
];

const LEADING_FILLER: &[&str] = &[
    "please", "can", "could", "would", "you", "kindly", "now", "just", "quick", "quickly", "hey",
    "hi", "hello",
];

/// Strict prompt-shape limits for the heuristic prefilter. Anything
/// outside these bounds falls through to the LLM judge (or `Parent`
/// when the judge is disabled), never directly to "cheap" — the goal
/// is to admit only the most obvious slam-dunks and let the judge
/// handle everything else.
const HEURISTIC_MAX_WORDS: usize = 15;
const HEURISTIC_MAX_SENTENCES: usize = 1;

const REFUSAL_PHRASES: &[&str] = &[
    "i'm not sure",
    "i am not sure",
    "i need more context",
    "need more context",
    "i don't have enough context",
    "i do not have enough context",
    "i cannot proceed",
    "i can't proceed",
];
const REFUSAL_TAIL_CHARS: usize = 96;

/// Heuristic prefilter — pure function. Returns the matched rule name
/// when the prompt is a slam-dunk for cheap routing, otherwise `None`.
///
/// Returning `None` does **not** mean "use the parent model"; it means
/// "let the next layer decide" — for borderline prompts within the
/// judge's char budget that translates to the LLM-judge call, and for
/// everything else to `Parent`. The bar here is deliberately strict so
/// the heuristic only fires on inputs the cheap tier handles
/// unambiguously: short, single-clause, single-imperative requests
/// naming the mechanical operation and its target. Anything longer,
/// compound, or vague gets the second-opinion judge.
pub(crate) fn heuristic_slam_dunk(user_input: &str, cfg: &RoutingConfig) -> Option<CheapReason> {
    let trimmed = user_input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if (trimmed.chars().count() as u32) > cfg.heuristic_max_chars {
        return None;
    }
    if trimmed.contains("\n\n") {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if AMBIGUITY_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return None;
    }
    if has_compound_signal(&lower) {
        return None;
    }
    if count_words(&lower) > HEURISTIC_MAX_WORDS {
        return None;
    }
    if count_sentences(trimmed) > HEURISTIC_MAX_SENTENCES {
        return None;
    }
    let mut words = lower
        .split(|c: char| !c.is_ascii_alphabetic() && c != '-')
        .filter(|word| !word.is_empty());
    let first = loop {
        let word = words.next()?;
        if !LEADING_FILLER.contains(&word) {
            break word;
        }
    };
    if let Some(verb) = HEURISTIC_VERBS.iter().copied().find(|verb| *verb == first) {
        return Some(CheapReason::HeuristicSlamDunk(verb));
    }
    // User-extended whitelist runs AFTER the builtin so adding a verb
    // can never override a hardcoded ambiguity marker (e.g. adding
    // "investigate" to the extra list still falls through to `None`
    // because the marker check earlier already rejected the prompt).
    for extra in &cfg.extra_heuristic_verbs {
        if extra.eq_ignore_ascii_case(first) {
            return Some(CheapReason::ExtraVerb(Arc::from(first)));
        }
    }
    None
}

fn count_words(lower: &str) -> usize {
    lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
        .filter(|word| !word.is_empty())
        .count()
}

fn normalized_words(lower: &str) -> Vec<&str> {
    lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
        .filter(|word| !word.is_empty())
        .collect()
}

fn has_compound_signal(lower: &str) -> bool {
    let words = normalized_words(lower);
    if words.is_empty() {
        return false;
    }
    if words.contains(&"then") || lower.contains(';') {
        return true;
    }
    if words.windows(2).any(|pair| pair == ["and", "check"]) {
        return true;
    }
    if words.windows(2).any(|pair| pair == ["and", "update"]) {
        return true;
    }
    if words.windows(2).any(|pair| pair == ["and", "verify"]) {
        return true;
    }
    if words.windows(2).any(|pair| pair == ["and", "confirm"]) {
        return true;
    }
    if words.windows(2).any(|pair| pair == ["and", "ensure"]) {
        return true;
    }
    if words
        .windows(3)
        .any(|triple| triple == ["and", "make", "sure"])
    {
        return true;
    }
    false
}

fn count_sentences(text: &str) -> usize {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return 0;
    }
    // Count terminators followed by whitespace OR end-of-string; treat
    // bare-comma compound asks as already filtered by COMPOUND_CONNECTORS
    // above so we don't have to teach this function about clause shape.
    let mut sentences = 0usize;
    let lower = trimmed.to_ascii_lowercase();
    let mut iter = trimmed.char_indices().peekable();
    while let Some((idx, ch)) = iter.next() {
        if matches!(ch, '.' | '!' | '?') {
            let next_is_uppercase = iter
                .clone()
                .find(|(_, next)| !next.is_whitespace())
                .is_some_and(|(_, next)| next.is_ascii_uppercase());
            if ch == '.' && period_is_abbreviation(&lower, idx) && !next_is_uppercase {
                continue;
            }
            match iter.peek() {
                Some((_, next)) if next.is_whitespace() || next.is_ascii_uppercase() => {
                    sentences += 1;
                }
                None => sentences += 1,
                _ => {}
            }
        }
    }
    if sentences == 0 {
        // Single declarative without a terminator counts as one sentence.
        return 1;
    }
    // A trailing terminator means we have N sentences; if the last
    // sentence has no terminator we still want to count it.
    let last = trimmed.chars().last();
    match last {
        Some('.') | Some('!') | Some('?') => sentences,
        _ => sentences + 1,
    }
}

fn period_is_abbreviation(lower: &str, period_idx: usize) -> bool {
    const ABBREVIATIONS: &[&str] = &["e.g.", "i.e.", "etc."];
    let end = period_idx + 1;
    ABBREVIATIONS.iter().any(|abbr| {
        let Some(start) = end.checked_sub(abbr.len()) else {
            return false;
        };
        lower.get(start..end) == Some(*abbr)
    })
}

/// True iff `text` contains any low-confidence phrase from the
/// assistant stream — used by the escalation detector.
pub(crate) fn contains_refusal_phrase(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    REFUSAL_PHRASES
        .iter()
        .any(|phrase| phrase_occurs_at_clause_start(&lower, phrase))
}

fn phrase_occurs_at_clause_start(text: &str, phrase: &str) -> bool {
    let mut search_from = 0usize;
    while let Some(offset) = text[search_from..].find(phrase) {
        let start = search_from + offset;
        let before = &text[..start];
        if before
            .chars()
            .rev()
            .find(|ch| !ch.is_whitespace())
            .is_none_or(|ch| matches!(ch, '.' | '!' | '?' | '\n' | '\r' | ':' | ';' | ','))
        {
            return true;
        }
        search_from = start.saturating_add(phrase.len());
    }
    false
}

#[derive(Debug, Deserialize)]
struct JudgeReply {
    route: String,
    #[serde(default, rename = "reason")]
    _reason: String,
}

/// Strict JSON-schema contract mirroring [`JudgeReply`]: a required `route`
/// constrained to the three tiers [`parse_judge_reply`] canonically emits plus
/// the `reason` the prompt asks for. Attached to the judge request only on
/// providers that forward `output_schema` ([`provider_honors_output_schema`])
/// so the judge returns a schema-valid object instead of fenced/prose-wrapped
/// JSON that costs a retry round — providers that drop the schema keep the
/// loose-parse path (which also still accepts the legacy `cheap`/`parent`).
fn judge_output_schema() -> LlmOutputSchema {
    LlmOutputSchema {
        name: "turn_route".to_string(),
        schema: serde_json::json!({
            "type": "object",
            "properties": {
                "route": { "type": "string", "enum": ["weak", "medium", "strong"] },
                "reason": { "type": "string" },
            },
            "required": ["route", "reason"],
            "additionalProperties": false,
        }),
        strict: true,
    }
}

pub(crate) struct ClassifyTurnInputs<'a> {
    pub user_input: &'a str,
    pub provider: &'a Arc<dyn LlmProvider>,
    pub provider_name: &'a str,
    pub parent_model: &'a str,
    /// The resolved cost/capability ladder for the active provider. Carries the
    /// concrete weak/medium/strong model ids the decision selects among.
    pub ladder: &'a TierLadder,
    pub config: &'a AppConfig,
    pub has_image_input: bool,
    pub has_large_attachment: bool,
    pub turn_index: u64,
    pub prior_turn_was_hard: bool,
    pub session_mode: SessionMode,
    pub overrides: RoutingOverride,
    pub sticky: bool,
    /// Mirror of `config.routing.linux_sandbox_sensitive_parent`. Passed
    /// explicitly so callers can override without touching the config.
    pub linux_sandbox_sensitive_parent: bool,
}

/// Result of classifying a single turn. Carries the routing decision
/// plus any cost the LLM judge actually billed — zero `CostSnapshot`
/// when the heuristic fired, the judge was disabled, the prompt fell
/// outside `judge_max_chars`, or the judge call errored before
/// emitting a `Completed` event.
pub(crate) struct ClassifyResult {
    pub decision: TurnRoutingDecision,
    pub judge_cost: CostSnapshot,
    /// The model the routing judge actually billed, set only when a judge
    /// call was dispatched. `None` on every early-return path that bills
    /// nothing (heuristic, disabled, sticky, short follow-up, …) so the
    /// caller attributes judge spend to the judge model and never invents a
    /// per-model ledger key for a call that did not happen.
    pub judge_model: Option<Arc<str>>,
}

impl ClassifyResult {
    fn parent() -> Self {
        Self {
            decision: TurnRoutingDecision::Parent,
            judge_cost: CostSnapshot::default(),
            judge_model: None,
        }
    }

    fn cheap(reason: CheapReason, tier: ModelTier, model: Arc<str>) -> Self {
        Self {
            decision: TurnRoutingDecision::Cheap {
                reason,
                tier,
                model,
            },
            judge_cost: CostSnapshot::default(),
            judge_model: None,
        }
    }
}

/// Whether a cheap-routed turn should run as a scoped cache-isolation subagent
/// (keeping the main loop on the parent model so its prompt cache stays warm)
/// instead of switching the main loop's model. `Switch` never isolates;
/// `Subagent` always does; `Auto` isolates only when prompt caching is supported
/// AND the assembled prefix already exceeds the configured token threshold — the
/// regime where a switch-and-escalate cold cache rewrite would dominate the
/// cheap savings. Below the threshold (or with no caching) the in-loop switch is
/// cheaper, so `Auto` matches today's behavior.
pub(crate) fn should_isolate(
    cfg: &RoutingConfig,
    prefix_tokens: u64,
    caching_supported: bool,
) -> bool {
    match cfg.cache_isolation {
        CacheIsolation::Switch => false,
        CacheIsolation::Subagent => true,
        CacheIsolation::Auto => {
            caching_supported && prefix_tokens > cfg.auto_prefix_token_threshold
        }
    }
}

pub(crate) async fn classify_turn(
    inputs: ClassifyTurnInputs<'_>,
    cancel: CancellationToken,
) -> ClassifyResult {
    let cfg = &inputs.config.routing;
    let ladder = inputs.ladder;

    if inputs.overrides.force_parent {
        return ClassifyResult::parent();
    }
    // The master switch (config or `/router off`) gates implicit
    // routing but never blocks an explicit `/cheap` request.
    let auto_disabled = inputs.overrides.session_disabled || !cfg.enabled;
    if auto_disabled && !inputs.overrides.force_cheap {
        return ClassifyResult::parent();
    }

    // Need a rung below the parent to reroute to at all. `can_reroute` is false
    // when the ladder collapsed to a single rung (no cheap tier, or the cheap
    // tier resolved to the parent itself) — routing would be a no-op.
    if !ladder.can_reroute() {
        return ClassifyResult::parent();
    }
    // The reroute filter decides whether this parent is worth rerouting. The
    // per-provider default reroutes every flagship while skipping already-cheap
    // tiers (haiku/mini/nano/flash) by name; users can override with their own
    // regex patterns (a leading `!` excludes). Resolved per provider, so it
    // never crosses providers; `/cheap` bypasses it.
    if !inputs.overrides.force_cheap {
        let filter = squeezy_core::resolved_reroute_filter(inputs.config, inputs.provider_name);
        if !squeezy_core::parent_is_reroute_eligible(inputs.parent_model, &filter) {
            return ClassifyResult::parent();
        }
    }
    // The cheapest rung available — what the heuristic and `/cheap` target.
    let (weak_tier, weak_model) = ladder.weakest();
    let weak_model: Arc<str> = Arc::from(weak_model);

    if inputs.session_mode == SessionMode::Plan {
        return ClassifyResult::parent();
    }
    if inputs.has_image_input && cfg.bypass_for_images {
        return ClassifyResult::parent();
    }
    if inputs.has_large_attachment {
        return ClassifyResult::parent();
    }

    if inputs.overrides.force_cheap {
        return ClassifyResult::cheap(CheapReason::UserExplicit, weak_tier, weak_model);
    }

    // Linux sandbox/container/kernel prompts go to the parent by default so the
    // cheap tier never mishandles host-sensitive work. An explicit `/cheap`
    // override (consumed above) still wins.
    if inputs.linux_sandbox_sensitive_parent && is_linux_sandbox_sensitive(inputs.user_input) {
        return ClassifyResult::parent();
    }

    // Short follow-ups inherit the prior turn's route instead of paying for a
    // judge call: an "ok"/"continue"/"that one" or any ultra-short prompt after
    // a hard (parent) turn stays on the parent, since it's almost always a
    // continuation of that task. Length-gated (`follow_up_max_chars`) so it
    // doesn't depend solely on a deictic word list, and free (no judge).
    if inputs.turn_index > 0
        && inputs.prior_turn_was_hard
        && (is_deictic_followup(inputs.user_input)
            || (inputs.user_input.trim().chars().count() as u32) <= cfg.follow_up_max_chars)
    {
        return ClassifyResult::parent();
    }

    if inputs.sticky {
        return ClassifyResult::parent();
    }

    if cfg.heuristic
        && let Some(reason) = heuristic_slam_dunk(inputs.user_input, cfg)
    {
        return ClassifyResult::cheap(reason, weak_tier, weak_model);
    }

    if !cfg.llm_judge {
        return ClassifyResult::parent();
    }
    let prompt_chars = inputs.user_input.chars().count() as u32;
    if prompt_chars == 0 || prompt_chars > cfg.judge_max_chars {
        return ClassifyResult::parent();
    }
    let judge_model = judge_model_for(inputs.provider_name, inputs.config, &weak_model);
    // Custom judge prompt (per-provider override → global) falls back to the
    // built-in per-provider instructions.
    let instructions = inputs
        .config
        .providers
        .get(inputs.provider_name)
        .and_then(|p| p.judge_prompt.as_deref())
        .or(cfg.judge_prompt.as_deref())
        .unwrap_or_else(|| judge_instructions_for(inputs.provider_name));
    let (verdict, judge_cost) = run_judge(
        inputs.provider,
        inputs.provider_name,
        &judge_model,
        instructions,
        inputs.user_input,
        cancel,
    )
    .await;
    match verdict {
        // The judge names a tier; clamp it up to the cheapest rung the ladder
        // actually populates at or above that difficulty. A `strong` verdict (or
        // a clamp that lands on the strong rung) keeps the turn on the parent.
        Some(tier) => {
            let (actual_tier, model) = ladder.at_least(tier);
            if actual_tier == ModelTier::Strong {
                ClassifyResult {
                    decision: TurnRoutingDecision::Parent,
                    judge_cost,
                    judge_model: Some(judge_model),
                }
            } else {
                ClassifyResult {
                    decision: TurnRoutingDecision::Cheap {
                        reason: CheapReason::LlmJudge,
                        tier: actual_tier,
                        model: Arc::from(model),
                    },
                    judge_cost,
                    judge_model: Some(judge_model),
                }
            }
        }
        None => ClassifyResult {
            decision: TurnRoutingDecision::Parent,
            judge_cost,
            judge_model: Some(judge_model),
        },
    }
}

fn judge_model_for(provider: &str, config: &AppConfig, cheap_model: &Arc<str>) -> Arc<str> {
    // Per-provider judge model wins, then the legacy global, then the
    // per-provider built-in mini tier (routing never crosses providers).
    let explicit = config
        .providers
        .get(provider)
        .and_then(|p| p.judge_model.clone())
        .filter(|m| !m.trim().is_empty())
        .or_else(|| config.routing.judge_model.clone());
    if let Some(model) = explicit {
        return Arc::from(
            squeezy_core::resolve_model_alias(provider, &model)
                .unwrap_or(&model)
                .to_string(),
        );
    }
    // No explicit judge model: default to the per-provider mini tier — a notch
    // above the cheapest reroute tier, which judges cheap-vs-parent far more
    // reliably (the nano tier tends to hedge). Falls back to the reroute model
    // for providers without a distinct mid tier.
    match squeezy_core::judge_model_for_provider(provider) {
        Some(mini) => Arc::from(mini.to_string()),
        None => cheap_model.clone(),
    }
}

/// Estimate the savings of running this turn on the cheap tier
/// instead of the parent model. Re-prices the actual provider-reported
/// `cost` (token counts) at the parent's per-Mtok rate via the same
/// `squeezy_llm::estimate_cost` helper the cap pre-flight uses, then
/// subtracts the cheap-tier bill. Returns `0` when either side has no
/// pricing entry in the registry — the field is best-effort.
pub(crate) fn estimate_routing_savings(
    provider: &str,
    parent_model: &str,
    actual_cheap_cost: &CostSnapshot,
) -> u64 {
    estimate_routing_net_savings(provider, parent_model, actual_cheap_cost, 0)
        .max(0)
        .try_into()
        .unwrap_or(0)
}

pub(crate) fn estimate_routing_net_savings(
    provider: &str,
    parent_model: &str,
    actual_cheap_cost: &CostSnapshot,
    judge_cost_usd_micros: u64,
) -> i64 {
    let Some(parent_estimate) =
        squeezy_llm::estimate_cost(provider, parent_model, actual_cheap_cost)
    else {
        return 0;
    };
    let actual = actual_cheap_cost.estimated_usd_micros.unwrap_or(0);
    (parent_estimate.min(i64::MAX as u64) as i64)
        .saturating_sub(actual.min(i64::MAX as u64) as i64)
        .saturating_sub(judge_cost_usd_micros.min(i64::MAX as u64) as i64)
}

// Built-in judge prompts live in squeezy-core (so the config screen can
// display "the prompt we're using"); a `[providers.<p>].judge_prompt`
// override is layered on at the call site in `classify_turn`.
fn judge_instructions_for(provider_name: &str) -> &'static str {
    squeezy_core::default_judge_prompt(provider_name)
}

const JUDGE_TIMEOUT_MS: u64 = 10_000;
const JUDGE_MAX_OUTPUT_TOKENS: u32 = 512;

async fn run_judge(
    provider: &Arc<dyn LlmProvider>,
    provider_name: &str,
    judge_model: &Arc<str>,
    instructions: &str,
    user_input: &str,
    cancel: CancellationToken,
) -> (Option<ModelTier>, CostSnapshot) {
    // The judge prompt is intentionally short. It sits below hosted
    // providers' useful prompt-cache minimums, so leave caching off
    // instead of surfacing misleading cache telemetry.
    let cache = CacheSpec {
        key: None,
        retention: CacheRetention::None,
    };
    let output_schema =
        provider_honors_output_schema(provider_name, judge_model).then(judge_output_schema);
    let request = LlmRequest {
        model: judge_model.clone(),
        instructions: Arc::from(instructions.to_string()),
        input: Arc::from(vec![LlmInputItem::UserText(user_input.to_string())]),
        max_output_tokens: Some(JUDGE_MAX_OUTPUT_TOKENS),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache,
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema,
        parallel_tool_calls: None,
        beta_headers: Arc::from(Vec::new()),
        ..LlmRequest::default()
    };
    let mut stream = provider.stream_response(request, cancel.clone());
    let fetch = async {
        let mut text = String::new();
        let mut cost = CostSnapshot::default();
        while let Some(event) = stream.next().await {
            match event {
                Ok(LlmEvent::TextDelta(delta)) => text.push_str(&delta),
                Ok(LlmEvent::Completed {
                    cost: completed_cost,
                    ..
                }) => {
                    cost = completed_cost;
                    break;
                }
                Ok(_) => continue,
                Err(_) => return (None, CostSnapshot::default()),
            }
        }
        (Some(text), cost)
    };
    let (raw, mut cost) = tokio::select! {
        biased;
        _ = cancel.cancelled() => return (None, CostSnapshot::default()),
        _ = tokio::time::sleep(Duration::from_millis(JUDGE_TIMEOUT_MS)) => {
            return (None, CostSnapshot::default());
        }
        result = fetch => result,
    };
    if cost.estimated_usd_micros.is_none() {
        cost.estimated_usd_micros = squeezy_llm::estimate_cost(provider_name, judge_model, &cost);
    }
    let verdict = raw.and_then(|text| parse_judge_reply(&text));
    (verdict, cost)
}

fn is_deictic_followup(user_input: &str) -> bool {
    let lower = user_input.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    const DEICTIC_MARKERS: &[&str] = &[
        "same",
        "keep going",
        "continue",
        "now do",
        "do that",
        "do the same",
        "again",
        "that one",
        "this one",
        "like that",
        "similar",
    ];
    DEICTIC_MARKERS
        .iter()
        .any(|marker| lower == *marker || lower.starts_with(&format!("{marker} ")))
}

fn parse_judge_reply(raw: &str) -> Option<ModelTier> {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|stripped| stripped.trim().trim_end_matches("```").trim())
        .unwrap_or(trimmed);
    let reply: JudgeReply = serde_json::from_str(body).ok()?;
    // `ModelTier::parse` accepts the canonical weak/medium/strong as well as the
    // legacy binary cheap/parent vocabulary, so an older judge prompt — or the
    // scripted test provider — keeps routing correctly.
    ModelTier::parse(reply.route.trim())
}

/// Per-turn escalation state. The streaming loop calls
/// [`Self::maybe_trigger`] after each tool result and each assistant
/// text flush; on `Some(reason)` the agent swaps to the parent model
/// for the rest of the turn and engages the sticky window.
#[derive(Debug, Default)]
pub(crate) struct EscalationState {
    pub triggered: Option<EscalationReason>,
    refusal_tail: String,
    /// Cumulative-counter snapshots taken when the turn entered the current
    /// rung. The detector compares against these deltas so each rung of a
    /// stepwise escalation (weak → medium → strong) gets its OWN tool-call and
    /// error budget rather than inheriting the cheaper rung's spend. Default
    /// `0` keeps the single-escalation path (and every existing unit test)
    /// identical: deltas equal the absolute cumulative counts.
    tool_calls_baseline: u64,
    errors_baseline: u64,
}

impl EscalationState {
    /// Re-arm the detector for the next rung after a stepwise escalation:
    /// clears the one-shot `triggered` latch and the refusal tail, and rebases
    /// the counters so the new (stronger) rung starts with a fresh budget.
    pub fn rearm_for_next_rung(&mut self, tool_calls: u64, tool_errors: u64, budget_denials: u64) {
        self.triggered = None;
        self.refusal_tail.clear();
        self.tool_calls_baseline = tool_calls;
        self.errors_baseline = tool_errors.saturating_add(budget_denials);
    }

    /// The detector intentionally takes seven orthogonal signals
    /// (three counters, the latest assistant text, the on-cheap-turn
    /// gate, the routing config, and the parent's tool budget) so the
    /// caller does not have to construct a transient struct just to
    /// poll for escalation on every round and every text delta. The
    /// clippy `too_many_arguments` lint flags the 8 args; we allow it
    /// here rather than introduce a wrapper type that would obscure
    /// the call-site contract.
    #[allow(clippy::too_many_arguments)]
    pub fn maybe_trigger(
        &mut self,
        tool_calls: u64,
        tool_errors: u64,
        budget_denials: u64,
        assistant_text_delta: &str,
        on_cheap_turn: bool,
        cfg: &RoutingConfig,
        max_tool_calls_per_turn: u64,
    ) -> Option<EscalationReason> {
        if !on_cheap_turn || self.triggered.is_some() {
            return None;
        }
        let ceiling = cfg.resolved_cheap_escalation_tool_calls(max_tool_calls_per_turn);
        if tool_calls.saturating_sub(self.tool_calls_baseline) > ceiling {
            self.triggered = Some(EscalationReason::ToolCallCeiling);
            return self.triggered;
        }
        let error_threshold = cfg.cheap_escalation_error_threshold as u64;
        let errors = tool_errors
            .saturating_add(budget_denials)
            .saturating_sub(self.errors_baseline);
        if errors >= error_threshold && error_threshold > 0 {
            self.triggered = Some(EscalationReason::ErrorThreshold);
            return self.triggered;
        }
        if self.observes_refusal_phrase(assistant_text_delta) {
            self.triggered = Some(EscalationReason::RefusalPhrase);
            return self.triggered;
        }
        None
    }

    fn observes_refusal_phrase(&mut self, assistant_text_delta: &str) -> bool {
        if assistant_text_delta.is_empty() {
            return false;
        }
        let mut window =
            String::with_capacity(self.refusal_tail.len() + assistant_text_delta.len());
        window.push_str(&self.refusal_tail);
        window.push_str(assistant_text_delta);
        let matched = contains_refusal_phrase(&window);
        self.refusal_tail = window
            .chars()
            .rev()
            .take(REFUSAL_TAIL_CHARS)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        matched
    }
}

#[cfg(test)]
#[path = "turn_router_tests.rs"]
mod tests;
