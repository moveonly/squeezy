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
use squeezy_core::{AppConfig, ReasoningEffort, RoutingConfig};
use squeezy_llm::{CacheSpec, LlmEvent, LlmInputItem, LlmProvider, LlmRequest};
use tokio_util::sync::CancellationToken;

use crate::cheap_model_for;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CheapReason {
    HeuristicSlamDunk(&'static str),
    LlmJudge,
    UserExplicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscalationReason {
    ToolCallCeiling,
    ErrorThreshold,
    RefusalPhrase,
}

impl EscalationReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ToolCallCeiling => "tool_call_ceiling",
            Self::ErrorThreshold => "error_threshold",
            Self::RefusalPhrase => "refusal_phrase",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum TurnRoutingDecision {
    Parent,
    Cheap {
        reason: CheapReason,
        model: Arc<str>,
    },
}

impl TurnRoutingDecision {
    pub(crate) fn is_cheap(&self) -> bool {
        matches!(self, Self::Cheap { .. })
    }

    pub(crate) fn reason_label(&self) -> Option<&'static str> {
        match self {
            Self::Cheap { reason, .. } => Some(match reason {
                CheapReason::HeuristicSlamDunk(rule) => rule,
                CheapReason::LlmJudge => "llm_judge",
                CheapReason::UserExplicit => "user_explicit",
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
];

const LEADING_FILLER: &[&str] = &[
    "please", "can", "could", "would", "you", "kindly", "now", "just", "quick", "quickly", "hey",
    "hi", "hello",
];

/// Conjunctions and connector phrases that signal a compound, multi-step
/// request when they sit between two verb-shaped clauses. We reject if
/// any appear after the imperative verb because the cheap tier handles
/// single mechanical asks much more reliably than compound ones (e.g.
/// "rename foo to bar, then check if any test fails, then update README"
/// is a classic borderline case the cheap model gets wrong).
const COMPOUND_CONNECTORS: &[&str] = &[
    ", then",
    " then ",
    "; then",
    "; and",
    "and check",
    "and update",
    "and verify",
    "and confirm",
    "and ensure",
    "and make sure",
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
    "this is complex",
    "need more context",
    "i can't",
    "i cannot",
    "unable to",
    "let me think",
];

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
pub(crate) fn heuristic_slam_dunk(user_input: &str, cfg: &RoutingConfig) -> Option<&'static str> {
    let trimmed = user_input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if (trimmed.len() as u32) > cfg.heuristic_max_chars {
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
    if COMPOUND_CONNECTORS
        .iter()
        .any(|connector| lower.contains(connector))
    {
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
    HEURISTIC_VERBS.iter().copied().find(|verb| *verb == first)
}

fn count_words(lower: &str) -> usize {
    lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
        .filter(|word| !word.is_empty())
        .count()
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
    let mut iter = trimmed.chars().peekable();
    while let Some(ch) = iter.next() {
        if matches!(ch, '.' | '!' | '?') {
            match iter.peek() {
                Some(next) if next.is_whitespace() => sentences += 1,
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

/// True iff `text` contains any low-confidence phrase from the
/// assistant stream — used by the escalation detector.
pub(crate) fn contains_refusal_phrase(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    REFUSAL_PHRASES.iter().any(|phrase| lower.contains(phrase))
}

#[derive(Debug, Deserialize)]
struct JudgeReply {
    route: String,
    #[serde(default, rename = "reason")]
    _reason: String,
}

pub(crate) struct ClassifyTurnInputs<'a> {
    pub user_input: &'a str,
    pub provider: &'a Arc<dyn LlmProvider>,
    pub provider_name: &'a str,
    pub parent_model: &'a str,
    pub config: &'a AppConfig,
    pub has_image_input: bool,
    pub overrides: RoutingOverride,
    pub sticky: bool,
}

pub(crate) async fn classify_turn(
    inputs: ClassifyTurnInputs<'_>,
    cancel: CancellationToken,
) -> TurnRoutingDecision {
    let cfg = &inputs.config.routing;

    if inputs.overrides.force_parent {
        return TurnRoutingDecision::Parent;
    }
    // The master switch (config or `/router off`) gates implicit
    // routing but never blocks an explicit `/cheap` request.
    let auto_disabled = inputs.overrides.session_disabled || !cfg.auto_cheap;
    if auto_disabled && !inputs.overrides.force_cheap {
        return TurnRoutingDecision::Parent;
    }
    if inputs.sticky {
        return TurnRoutingDecision::Parent;
    }
    if inputs.has_image_input && cfg.bypass_for_images {
        return TurnRoutingDecision::Parent;
    }

    let Some(cheap) = cheap_model_for(inputs.provider_name, inputs.config) else {
        return TurnRoutingDecision::Parent;
    };
    if cheap == inputs.parent_model {
        // Routing to the same model would be a no-op — skip the
        // classifier and the judge call entirely.
        return TurnRoutingDecision::Parent;
    }
    let cheap: Arc<str> = Arc::from(cheap);

    if inputs.overrides.force_cheap {
        return TurnRoutingDecision::Cheap {
            reason: CheapReason::UserExplicit,
            model: cheap,
        };
    }

    if let Some(rule) = heuristic_slam_dunk(inputs.user_input, cfg) {
        return TurnRoutingDecision::Cheap {
            reason: CheapReason::HeuristicSlamDunk(rule),
            model: cheap,
        };
    }

    if !cfg.auto_cheap_llm_judge {
        return TurnRoutingDecision::Parent;
    }
    let prompt_chars = inputs.user_input.chars().count() as u32;
    if prompt_chars == 0 || prompt_chars > cfg.judge_max_chars {
        return TurnRoutingDecision::Parent;
    }
    match run_judge(inputs.provider, &cheap, inputs.user_input, cancel).await {
        Some(true) => TurnRoutingDecision::Cheap {
            reason: CheapReason::LlmJudge,
            model: cheap,
        },
        _ => TurnRoutingDecision::Parent,
    }
}

const JUDGE_INSTRUCTIONS: &str = concat!(
    "You are a routing classifier deciding which LLM should handle a coding-agent turn. ",
    "The parent model is expensive but excellent at multi-step reasoning. The cheap model is fast and ",
    "inexpensive but weaker at ambiguous instructions and architectural judgement. ",
    "Reply with a SINGLE JSON object on one line, no markdown, no prose: ",
    "{\"route\":\"cheap\"|\"parent\",\"reason\":\"<short explanation>\"}.\n\n",
    "Choose 'cheap' when the request is well-specified, narrowly scoped, and mechanical — a single named ",
    "operation plus its targets (e.g. \"checkout branch X and run cargo test\", \"rename foo() to bar() in src/lib.rs\"). ",
    "Choose 'parent' when the request needs architectural reasoning, cross-file synthesis, exploratory ",
    "investigation, debugging, or judgement about trade-offs. When in doubt, choose 'parent'.",
);

const JUDGE_TIMEOUT_MS: u64 = 10_000;
const JUDGE_MAX_OUTPUT_TOKENS: u32 = 80;

async fn run_judge(
    provider: &Arc<dyn LlmProvider>,
    cheap_model: &Arc<str>,
    user_input: &str,
    cancel: CancellationToken,
) -> Option<bool> {
    let request = LlmRequest {
        model: cheap_model.clone(),
        instructions: Arc::from(JUDGE_INSTRUCTIONS.to_string()),
        input: Arc::from(vec![LlmInputItem::UserText(user_input.to_string())]),
        max_output_tokens: Some(JUDGE_MAX_OUTPUT_TOKENS),
        response_verbosity: None,
        reasoning_effort: Some(ReasoningEffort::Low),
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: Arc::from(Vec::new()),
    };
    let mut stream = provider.stream_response(request, cancel.clone());
    let fetch = async {
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(LlmEvent::TextDelta(delta)) => text.push_str(&delta),
                Ok(LlmEvent::Completed { .. }) => break,
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
        Some(text)
    };
    let raw = tokio::select! {
        biased;
        _ = cancel.cancelled() => return None,
        _ = tokio::time::sleep(Duration::from_millis(JUDGE_TIMEOUT_MS)) => return None,
        result = fetch => result?,
    };
    parse_judge_reply(&raw)
}

fn parse_judge_reply(raw: &str) -> Option<bool> {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|stripped| stripped.trim().trim_end_matches("```").trim())
        .unwrap_or(trimmed);
    let reply: JudgeReply = serde_json::from_str(body).ok()?;
    match reply.route.trim().to_ascii_lowercase().as_str() {
        "cheap" => Some(true),
        "parent" => Some(false),
        _ => None,
    }
}

/// Per-turn escalation state. The streaming loop calls
/// [`Self::maybe_trigger`] after each tool result and each assistant
/// text flush; on `Some(reason)` the agent swaps to the parent model
/// for the rest of the turn and engages the sticky window.
#[derive(Debug, Default)]
pub(crate) struct EscalationState {
    pub triggered: Option<EscalationReason>,
}

impl EscalationState {
    pub fn maybe_trigger(
        &mut self,
        tool_calls: u64,
        tool_errors: u64,
        budget_denials: u64,
        recent_assistant_text: &str,
        on_cheap_turn: bool,
        cfg: &RoutingConfig,
        max_tool_calls_per_turn: u64,
    ) -> Option<EscalationReason> {
        if !on_cheap_turn || self.triggered.is_some() {
            return None;
        }
        let ceiling = cfg.resolved_cheap_escalation_tool_calls(max_tool_calls_per_turn);
        if tool_calls > ceiling {
            self.triggered = Some(EscalationReason::ToolCallCeiling);
            return self.triggered;
        }
        let error_threshold = cfg.cheap_escalation_error_threshold as u64;
        if tool_errors.saturating_add(budget_denials) >= error_threshold && error_threshold > 0 {
            self.triggered = Some(EscalationReason::ErrorThreshold);
            return self.triggered;
        }
        if contains_refusal_phrase(recent_assistant_text) {
            self.triggered = Some(EscalationReason::RefusalPhrase);
            return self.triggered;
        }
        None
    }
}

#[cfg(test)]
#[path = "turn_router_tests.rs"]
mod tests;
