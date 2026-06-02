use squeezy_core::{
    CostSnapshot, DEFAULT_ROUTING_HEURISTIC_MAX_CHARS, DEFAULT_ROUTING_JUDGE_MAX_CHARS,
    RoutingConfig,
};

use super::{
    CheapReason, EscalationReason, EscalationState, contains_refusal_phrase,
    estimate_routing_savings,
};

/// Test-only wrapper that keeps the previous `Option<&'static str>`
/// shape so existing assertions continue to read as
/// `Some("verb") | None`. Built-in matches surface their static verb;
/// extra-verb matches surface `None` here (covered by dedicated tests
/// below that target the `ExtraVerb` variant explicitly).
fn heuristic_slam_dunk(input: &str, cfg: &RoutingConfig) -> Option<&'static str> {
    match super::heuristic_slam_dunk(input, cfg) {
        Some(CheapReason::HeuristicSlamDunk(verb)) => Some(verb),
        _ => None,
    }
}

fn default_routing_config() -> RoutingConfig {
    RoutingConfig {
        auto_cheap: true,
        auto_cheap_llm_judge: true,
        cheap_escalation_tool_calls: 0,
        cheap_escalation_error_threshold: 2,
        escalation_sticky_turns: 3,
        bypass_for_images: true,
        heuristic_max_chars: DEFAULT_ROUTING_HEURISTIC_MAX_CHARS,
        judge_max_chars: DEFAULT_ROUTING_JUDGE_MAX_CHARS,
        extra_heuristic_verbs: Vec::new(),
    }
}

// -- True positives --------------------------------------------------------
//
// Prompts the cheap tier should handle reliably: single mechanical
// imperative, named target, fewer than `HEURISTIC_MAX_WORDS` words,
// one or two sentences, no compound connectors. These all match.

#[test]
fn heuristic_matches_single_imperative_with_target() {
    let cfg = default_routing_config();
    assert_eq!(
        heuristic_slam_dunk("run cargo test -p squeezy-llm", &cfg),
        Some("run")
    );
    assert_eq!(heuristic_slam_dunk("ls src/squeezy-llm", &cfg), Some("ls"));
    assert_eq!(heuristic_slam_dunk("cat /etc/hosts", &cfg), Some("cat"));
    assert_eq!(
        heuristic_slam_dunk("grep TODO src/lib.rs", &cfg),
        Some("grep")
    );
}

#[test]
fn heuristic_matches_with_polite_filler() {
    let cfg = default_routing_config();
    assert_eq!(
        heuristic_slam_dunk("please run cargo fmt", &cfg),
        Some("run")
    );
    assert_eq!(
        heuristic_slam_dunk("Could you checkout main", &cfg),
        Some("checkout")
    );
}

// -- True negatives: ambiguity markers -------------------------------------

#[test]
fn heuristic_rejects_ambiguity_markers() {
    let cfg = default_routing_config();
    assert_eq!(
        heuristic_slam_dunk("maybe rename this function", &cfg),
        None
    );
    assert_eq!(
        heuristic_slam_dunk("decide whether to checkout main", &cfg),
        None
    );
    assert_eq!(
        heuristic_slam_dunk("figure out which tests run", &cfg),
        None
    );
    assert_eq!(
        heuristic_slam_dunk("investigate the failing test", &cfg),
        None
    );
}

// -- True negatives: layout / length ---------------------------------------

#[test]
fn heuristic_rejects_multi_paragraph() {
    let cfg = default_routing_config();
    let prompt = "run cargo test\n\nthen update the README with the results";
    assert_eq!(heuristic_slam_dunk(prompt, &cfg), None);
}

#[test]
fn heuristic_rejects_long_prompts_over_char_budget() {
    let mut cfg = default_routing_config();
    cfg.heuristic_max_chars = 50;
    let long =
        "run a very long sentence that goes well past the configured budget for the prefilter";
    assert_eq!(heuristic_slam_dunk(long, &cfg), None);
}

#[test]
fn heuristic_rejects_too_many_words() {
    let cfg = default_routing_config();
    let wordy =
        "run cargo test with the flag that enables the new feature on the staging branch please";
    assert_eq!(
        heuristic_slam_dunk(wordy, &cfg),
        None,
        "wordy prompts must defer to the judge"
    );
}

#[test]
fn heuristic_rejects_too_many_sentences() {
    let cfg = default_routing_config();
    let multi_sentence = "run cargo test. Then commit. Then push.";
    assert_eq!(
        heuristic_slam_dunk(multi_sentence, &cfg),
        None,
        ">2 sentences must defer to the judge"
    );
}

// -- True negatives: compound asks (false-positive guards) -----------------

#[test]
fn heuristic_rejects_compound_then_clauses() {
    let cfg = default_routing_config();
    assert_eq!(
        heuristic_slam_dunk("rename foo to bar, then check if any test fails", &cfg),
        None,
        "comma-then compound must defer"
    );
    assert_eq!(
        heuristic_slam_dunk("run cargo fmt and check the diff", &cfg),
        None,
        "and-check compound must defer"
    );
    assert_eq!(
        heuristic_slam_dunk("run the test and verify the output", &cfg),
        None,
        "and-verify compound must defer"
    );
}

#[test]
fn heuristic_rejects_ambiguous_scope_targets() {
    let cfg = default_routing_config();
    // "any test" / "any file" markers signal the cheap tier would
    // have to enumerate and pick — judge territory.
    assert_eq!(
        heuristic_slam_dunk("run any test that touches auth", &cfg),
        None
    );
    assert_eq!(
        heuristic_slam_dunk("rename any file with the old prefix", &cfg),
        None
    );
    // "legacy" implies cross-file reasoning even though the imperative is clean.
    assert_eq!(
        heuristic_slam_dunk("delete the legacy auth module", &cfg),
        None
    );
}

// -- True negatives: unknown imperative ------------------------------------

#[test]
fn heuristic_rejects_unknown_imperative() {
    let cfg = default_routing_config();
    assert_eq!(
        heuristic_slam_dunk("explain how the cost broker works", &cfg),
        None
    );
    assert_eq!(
        heuristic_slam_dunk("write a function that does X", &cfg),
        None
    );
    // 'delete' deliberately not whitelisted — risk of "delete unused
    // X across the repo" needing reasoning.
    assert_eq!(
        heuristic_slam_dunk("delete the file src/bad.rs", &cfg),
        None
    );
    // 'refactor' is never simple.
    assert_eq!(heuristic_slam_dunk("refactor the cost broker", &cfg), None);
    // 'add' / 'remove' compound too often with reasoning targets.
    assert_eq!(
        heuristic_slam_dunk("add a new field to AppConfig", &cfg),
        None
    );
}

// -- Refusal-phrase detector -----------------------------------------------

#[test]
fn refusal_phrases_detected_case_insensitively() {
    assert!(contains_refusal_phrase("Hmm, I'm not sure how to proceed."));
    assert!(contains_refusal_phrase(
        "I cannot proceed without more context."
    ));
    assert!(contains_refusal_phrase(
        "Need more context before I continue."
    ));
    assert!(!contains_refusal_phrase("Running cargo test now."));
    assert!(!contains_refusal_phrase(
        "The linker is unable to find libfoo."
    ));
    assert!(!contains_refusal_phrase(
        "I can't reproduce the panic, so the fix is good."
    ));
    assert!(!contains_refusal_phrase(
        "This is complex, so I checked the call graph."
    ));
    assert!(!contains_refusal_phrase(""));
}

#[test]
fn refusal_detector_handles_phrases_split_across_deltas() {
    let cfg = default_routing_config();
    let mut state = EscalationState::default();
    assert_eq!(
        state.maybe_trigger(0, 0, 0, "I'm not", true, &cfg, 10_000),
        None
    );
    assert_eq!(
        state.maybe_trigger(0, 0, 0, " sure how to continue.", true, &cfg, 10_000),
        Some(EscalationReason::RefusalPhrase)
    );
}

// -- Escalation detector ---------------------------------------------------

#[test]
fn escalation_fires_on_tool_call_ceiling() {
    let cfg = default_routing_config();
    let mut state = EscalationState::default();
    let triggered = state.maybe_trigger(3000, 0, 0, "", true, &cfg, 10_000);
    assert_eq!(triggered, Some(EscalationReason::ToolCallCeiling));
}

#[test]
fn escalation_fires_on_error_threshold() {
    let cfg = default_routing_config();
    let mut state = EscalationState::default();
    let triggered = state.maybe_trigger(0, 1, 1, "", true, &cfg, 10_000);
    assert_eq!(triggered, Some(EscalationReason::ErrorThreshold));
}

#[test]
fn escalation_fires_on_refusal_phrase() {
    let cfg = default_routing_config();
    let mut state = EscalationState::default();
    let triggered = state.maybe_trigger(0, 0, 0, "I'm not sure", true, &cfg, 10_000);
    assert_eq!(triggered, Some(EscalationReason::RefusalPhrase));
}

#[test]
fn escalation_skips_parent_turn() {
    let cfg = default_routing_config();
    let mut state = EscalationState::default();
    let triggered = state.maybe_trigger(9000, 5, 5, "I'm not sure", false, &cfg, 10_000);
    assert_eq!(triggered, None);
}

#[test]
fn escalation_latches_once() {
    let cfg = default_routing_config();
    let mut state = EscalationState::default();
    let first = state.maybe_trigger(3000, 0, 0, "", true, &cfg, 10_000);
    assert!(first.is_some());
    let second = state.maybe_trigger(3000, 0, 0, "I'm not sure", true, &cfg, 10_000);
    assert_eq!(second, None, "must not re-fire once triggered");
}

#[test]
fn sticky_window_expires_after_n_turns() {
    let mut sticky = super::StickyEscalation::default();
    sticky.engage(3);
    assert!(sticky.tick());
    assert!(sticky.tick());
    assert!(sticky.tick());
    assert!(!sticky.tick(), "fourth turn must not be sticky");
}

// -- Judge reply parsing ---------------------------------------------------

#[test]
fn parse_judge_reply_handles_bare_json() {
    let parsed = super::parse_judge_reply(r#"{"route":"cheap","reason":"single command"}"#);
    assert_eq!(parsed, Some(true));
}

#[test]
fn parse_judge_reply_handles_code_fence() {
    let raw = "```json\n{\"route\":\"parent\",\"reason\":\"needs reasoning\"}\n```";
    assert_eq!(super::parse_judge_reply(raw), Some(false));
}

#[test]
fn parse_judge_reply_rejects_garbage() {
    assert_eq!(super::parse_judge_reply(""), None);
    assert_eq!(super::parse_judge_reply("nope"), None);
    assert_eq!(
        super::parse_judge_reply(r#"{"route":"unknown","reason":"-"}"#),
        None
    );
}

// -- Cheap-escalation derived ceiling --------------------------------------

#[test]
fn resolved_cheap_escalation_uses_quarter_of_parent_budget() {
    let cfg = default_routing_config();
    assert_eq!(cfg.resolved_cheap_escalation_tool_calls(40), 10);
    assert_eq!(cfg.resolved_cheap_escalation_tool_calls(10_000), 2500);
}

#[test]
fn resolved_cheap_escalation_honors_explicit_override() {
    let mut cfg = default_routing_config();
    cfg.cheap_escalation_tool_calls = 7;
    assert_eq!(cfg.resolved_cheap_escalation_tool_calls(10_000), 7);
}

// -- Adversarial false-positive corpus --------------------------------------
//
// This list is the reliability gate. Each entry is a prompt that *looks*
// simple but actually carries hidden complexity the cheap tier would
// typically get wrong. If the heuristic regresses and starts saying
// "cheap" on any of these, those false positives would silently waste a
// cheap-model call before the escalation detector caught up — better to
// defer to the judge from the start.

const ADVERSARIAL_FALSE_POSITIVE_PROMPTS: &[&str] = &[
    // Multi-step compound asks
    "run cargo test then update the README with the results",
    "rename foo to bar, then check if any tests fail, then update README",
    "checkout main and ensure the build still passes",
    "run fmt and verify nothing changed",
    "fetch origin and confirm we're up to date",
    // Vague scope inside a clean imperative
    "delete the legacy auth module",
    "rename any file with the old prefix",
    "run any test touching auth",
    "grep for the one that handles refresh tokens",
    // Reasoning-needed verbs that aren't in the whitelist anyway
    "refactor the cost broker into smaller modules",
    "add a new field to AppConfig for routing",
    "investigate why CI is failing on macOS",
    "explain how the cost broker tracks budgets",
    // Decision-needed phrasing
    "should we checkout the staging branch first",
    "what if we run the tests in parallel",
    "decide which model to use for the next turn",
    // Multi-sentence asks
    "run cargo test. Then commit. Then push to main.",
    "rename foo to bar. Confirm with cargo check.",
    // Long but otherwise-clean asks (wordy)
    "run cargo test with the flag that enables the new feature on the staging branch please",
];

#[test]
fn heuristic_rejects_every_adversarial_false_positive() {
    let cfg = default_routing_config();
    for prompt in ADVERSARIAL_FALSE_POSITIVE_PROMPTS {
        assert_eq!(
            heuristic_slam_dunk(prompt, &cfg),
            None,
            "heuristic must defer to the judge on adversarial prompt: {prompt:?}",
        );
    }
}

// True positives — these MUST keep firing on the heuristic so the
// router doesn't burn an LLM-judge call on every trivial ask.

const TRUE_POSITIVE_PROMPTS: &[(&str, &str)] = &[
    ("run cargo test", "run"),
    ("ls src", "ls"),
    ("cat README.md", "cat"),
    ("grep TODO src/lib.rs", "grep"),
    ("checkout main", "checkout"),
    ("rename foo to bar in src/lib.rs", "rename"),
    ("format src/lib.rs", "format"),
    ("fmt", "fmt"),
    ("lint", "lint"),
    ("please run cargo fmt", "run"),
];

#[test]
fn heuristic_fires_on_each_true_positive() {
    let cfg = default_routing_config();
    for (prompt, expected_verb) in TRUE_POSITIVE_PROMPTS {
        let outcome = heuristic_slam_dunk(prompt, &cfg);
        assert_eq!(
            outcome,
            Some(*expected_verb),
            "true positive lost: {prompt:?}"
        );
    }
}

// -- Sentence / word counter sanity ----------------------------------------

#[test]
fn count_words_handles_punctuation() {
    assert_eq!(super::count_words("run cargo test"), 3);
    assert_eq!(super::count_words("run cargo-test"), 2);
    assert_eq!(super::count_words("run, cargo, test"), 3);
    assert_eq!(super::count_words(""), 0);
}

#[test]
fn count_sentences_handles_terminators() {
    assert_eq!(super::count_sentences("run cargo test"), 1);
    assert_eq!(super::count_sentences("run cargo test."), 1);
    assert_eq!(super::count_sentences("run cargo test. then commit."), 2);
    assert_eq!(super::count_sentences("first. second. third."), 3);
    assert_eq!(super::count_sentences("first? then second!"), 2);
}

// -- Per-provider judge prompts --------------------------------------------

#[test]
fn judge_instructions_default_used_for_anthropic_bedrock_etc() {
    let default = super::judge_instructions_for("anthropic");
    assert_eq!(default, super::judge_instructions_for("bedrock"));
    assert_eq!(default, super::judge_instructions_for("openrouter"));
    assert_eq!(default, super::judge_instructions_for("vercel"));
    assert_eq!(default, super::judge_instructions_for("portkey"));
    assert_eq!(default, super::judge_instructions_for("unknown-provider"));
}

#[test]
fn judge_instructions_openai_emphasises_no_prose() {
    let openai = super::judge_instructions_for("openai");
    assert_eq!(openai, super::judge_instructions_for("openai_codex"));
    assert_eq!(openai, super::judge_instructions_for("azure_openai"));
    assert!(
        openai.contains("ONLY"),
        "openai variant must emphasise no-prose: {openai}"
    );
    assert_ne!(openai, super::judge_instructions_for("anthropic"));
}

#[test]
fn judge_instructions_google_forbids_markdown_fences() {
    let google = super::judge_instructions_for("google");
    assert!(
        google.contains("NO markdown") || google.contains("NO code blocks"),
        "google variant must forbid markdown fences: {google}"
    );
    assert_ne!(google, super::judge_instructions_for("anthropic"));
}

#[test]
fn judge_instructions_all_variants_keep_routing_guidance() {
    for provider in ["anthropic", "openai", "google"] {
        let prompt = super::judge_instructions_for(provider);
        assert!(
            prompt.contains("'cheap'") && prompt.contains("'parent'"),
            "{provider} variant must carry the cheap/parent guidance: {prompt}"
        );
        assert!(
            prompt.contains("JSON"),
            "{provider} variant must instruct JSON output: {prompt}"
        );
    }
}

// -- User-extended heuristic whitelist -------------------------------------

fn cfg_with_extras(extras: &[&str]) -> RoutingConfig {
    let mut cfg = default_routing_config();
    cfg.extra_heuristic_verbs = extras.iter().map(|s| s.to_string()).collect();
    cfg
}

#[test]
fn extra_verb_fires_when_first_word_matches_user_list() {
    let cfg = cfg_with_extras(&["deploy"]);
    match super::heuristic_slam_dunk("deploy to staging", &cfg) {
        Some(CheapReason::ExtraVerb(verb)) => assert_eq!(&*verb, "deploy"),
        other => panic!("expected ExtraVerb match, got {other:?}"),
    }
}

#[test]
fn extra_verb_match_is_case_insensitive() {
    let cfg = cfg_with_extras(&["Deploy"]);
    match super::heuristic_slam_dunk("DEPLOY to staging", &cfg) {
        Some(CheapReason::ExtraVerb(verb)) => assert_eq!(&*verb, "deploy"),
        other => panic!("expected ExtraVerb match, got {other:?}"),
    }
}

#[test]
fn extra_verb_still_subject_to_ambiguity_marker_guard() {
    // Adding "investigate" to the user list cannot override the
    // hardcoded ambiguity marker — the prompt is rejected before the
    // verb check fires.
    let cfg = cfg_with_extras(&["investigate"]);
    assert!(
        super::heuristic_slam_dunk("investigate the failing test", &cfg).is_none(),
        "extra verb must not override the ambiguity marker guard"
    );
}

#[test]
fn extra_verb_still_subject_to_compound_connector_guard() {
    let cfg = cfg_with_extras(&["deploy"]);
    assert!(
        super::heuristic_slam_dunk("deploy to staging and verify the rollout", &cfg).is_none(),
        "extra verb must not override the compound connector guard"
    );
}

#[test]
fn extra_verb_does_not_shadow_builtin_match() {
    // Built-ins check first; adding "run" to the extra list is fine
    // but the matched reason should be the built-in `HeuristicSlamDunk`
    // (with `&'static str` payload) rather than the dynamic
    // `ExtraVerb` variant.
    let cfg = cfg_with_extras(&["run"]);
    match super::heuristic_slam_dunk("run cargo test", &cfg) {
        Some(CheapReason::HeuristicSlamDunk(verb)) => assert_eq!(verb, "run"),
        other => panic!("expected built-in match, got {other:?}"),
    }
}

// -- Routing savings math --------------------------------------------------

fn cost_with(input_tokens: u64, output_tokens: u64) -> CostSnapshot {
    CostSnapshot {
        input_tokens: Some(input_tokens),
        output_tokens: Some(output_tokens),
        estimated_usd_micros: squeezy_llm::estimate_cost(
            "anthropic",
            "claude-haiku-4-5-20251001",
            &CostSnapshot {
                input_tokens: Some(input_tokens),
                output_tokens: Some(output_tokens),
                ..Default::default()
            },
        ),
        ..Default::default()
    }
}

#[test]
fn estimate_routing_savings_returns_positive_when_both_priced() {
    let cheap_cost = cost_with(10_000, 1_000);
    let savings = estimate_routing_savings("anthropic", "claude-opus-4-7", &cheap_cost);
    assert!(
        savings > 0,
        "running cheap cost on Opus rates must cost more than Haiku, savings={savings}",
    );
}

#[test]
fn estimate_routing_savings_returns_zero_when_parent_unpriced() {
    let cheap_cost = cost_with(10_000, 1_000);
    // Bogus model id has no registry entry, so estimate_cost returns None.
    let savings = estimate_routing_savings("anthropic", "nonexistent-parent-model", &cheap_cost);
    assert_eq!(savings, 0);
}

#[test]
fn estimate_routing_savings_returns_zero_when_provider_unknown() {
    let cheap_cost = cost_with(10_000, 1_000);
    let savings = estimate_routing_savings("nonexistent-provider", "claude-opus-4-7", &cheap_cost);
    assert_eq!(savings, 0);
}

#[test]
fn estimate_routing_savings_zero_when_actual_already_above_parent_estimate() {
    // Degenerate case: cheap turn somehow billed more than parent
    // would have. saturating_sub keeps the result non-negative; we
    // expect 0 rather than panic / wrap-around.
    let mut cheap_cost = cost_with(10, 0);
    cheap_cost.estimated_usd_micros = Some(u64::MAX); // pretend cheap cost is enormous
    let savings = estimate_routing_savings("anthropic", "claude-opus-4-7", &cheap_cost);
    assert_eq!(savings, 0);
}
