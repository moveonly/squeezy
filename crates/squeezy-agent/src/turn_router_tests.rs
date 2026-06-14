use squeezy_core::{
    CostSnapshot, DEFAULT_ROUTING_HEURISTIC_MAX_CHARS, DEFAULT_ROUTING_JUDGE_MAX_CHARS, ModelTier,
    RoutingConfig,
};

use super::{
    CheapReason, EscalationReason, EscalationState, contains_refusal_phrase,
    estimate_routing_net_savings, estimate_routing_savings,
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
        enabled: true,
        heuristic: true,
        llm_judge: true,
        follow_up_max_chars: squeezy_core::DEFAULT_ROUTING_FOLLOW_UP_MAX_CHARS,
        judge_prompt: None,
        expensive_models: String::new(),
        cheap_escalation_tool_calls: 0,
        cheap_escalation_error_threshold: 2,
        escalation_sticky_turns: 3,
        bypass_for_images: true,
        large_attachment_bypass_bytes: squeezy_core::DEFAULT_ROUTING_LARGE_ATTACHMENT_BYPASS_BYTES,
        heuristic_max_chars: DEFAULT_ROUTING_HEURISTIC_MAX_CHARS,
        judge_max_chars: DEFAULT_ROUTING_JUDGE_MAX_CHARS,
        judge_model: None,
        extra_heuristic_verbs: Vec::new(),
        linux_sandbox_sensitive_parent:
            squeezy_core::DEFAULT_ROUTING_LINUX_SANDBOX_SENSITIVE_PARENT,
        cache_isolation: squeezy_core::DEFAULT_ROUTING_CACHE_ISOLATION,
        auto_prefix_token_threshold: squeezy_core::DEFAULT_ROUTING_AUTO_PREFIX_TOKEN_THRESHOLD,
        tier_effort: squeezy_core::DEFAULT_ROUTING_TIER_EFFORT,
        effort_weak: None,
        effort_medium: None,
        effort_strong: None,
        judge_effort: squeezy_core::DEFAULT_ROUTING_JUDGE_EFFORT,
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
fn heuristic_length_budget_counts_chars_not_bytes() {
    let mut cfg = default_routing_config();
    cfg.heuristic_max_chars = 5;
    assert_eq!(heuristic_slam_dunk("run \u{00e9}", &cfg), Some("run"));
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
    assert_eq!(
        heuristic_slam_dunk("rename foo,then run cargo test", &cfg),
        None,
        "comma-then without whitespace must defer"
    );
    assert_eq!(
        heuristic_slam_dunk("checkout main; deploy prod", &cfg),
        None,
        "semicolon compound with second step verb must defer"
    );
    assert_eq!(
        heuristic_slam_dunk("run test.Then commit.Then push", &cfg),
        None,
        "sentence chain without whitespace must defer"
    );
}

#[test]
fn heuristic_does_not_match_compound_connector_inside_words() {
    let cfg = default_routing_config();
    assert_eq!(
        heuristic_slam_dunk("run the command checker", &cfg),
        Some("run"),
        "connector words must be token-aware"
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

#[test]
fn heuristic_rejects_care_and_effort_cues() {
    let cfg = default_routing_config();
    assert_eq!(
        heuristic_slam_dunk("rename foo to bar carefully", &cfg),
        None
    );
    assert_eq!(heuristic_slam_dunk("run cargo test safely", &cfg), None);
    assert_eq!(
        heuristic_slam_dunk("checkout main without breaking local changes", &cfg),
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
fn rearm_gives_each_rung_its_own_tool_call_budget() {
    // Default config derives the ceiling as max_tool_calls / 4; with max=40 the
    // per-rung ceiling is 10, so escalation fires at the 11th call past the
    // rung's baseline.
    let cfg = default_routing_config();
    let mut state = EscalationState::default();
    assert_eq!(
        state.maybe_trigger(11, 0, 0, "", true, &cfg, 40),
        Some(EscalationReason::ToolCallCeiling)
    );
    // Latched until re-armed, even far past the ceiling.
    assert_eq!(state.maybe_trigger(50, 0, 0, "", true, &cfg, 40), None);
    // Re-arm for the next rung at the current cumulative count: the new rung
    // starts with a fresh budget measured as a delta from 11.
    state.rearm_for_next_rung(11, 0, 0);
    assert_eq!(
        state.maybe_trigger(21, 0, 0, "", true, &cfg, 40),
        None,
        "delta of 10 is at the ceiling, not over it"
    );
    assert_eq!(
        state.maybe_trigger(22, 0, 0, "", true, &cfg, 40),
        Some(EscalationReason::ToolCallCeiling),
        "delta of 11 trips the next rung"
    );
}

#[test]
fn rearm_gives_each_rung_its_own_error_budget() {
    let cfg = default_routing_config(); // error threshold = 2
    let mut state = EscalationState::default();
    assert_eq!(
        state.maybe_trigger(0, 2, 0, "", true, &cfg, 40),
        Some(EscalationReason::ErrorThreshold)
    );
    state.rearm_for_next_rung(0, 2, 0);
    assert_eq!(
        state.maybe_trigger(0, 3, 0, "", true, &cfg, 40),
        None,
        "one new error after re-arm is below the threshold"
    );
    assert_eq!(
        state.maybe_trigger(0, 4, 0, "", true, &cfg, 40),
        Some(EscalationReason::ErrorThreshold)
    );
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

/// The tier a judge reply parses to (dropping any effort), for tests that only
/// care about routing.
fn parsed_tier(raw: &str) -> Option<ModelTier> {
    super::parse_judge_reply(raw).map(|verdict| verdict.tier)
}

#[test]
fn parse_judge_reply_handles_bare_json() {
    // Legacy "cheap" maps to the weak tier; the new vocabulary parses directly.
    assert_eq!(
        parsed_tier(r#"{"route":"cheap","reason":"single command"}"#),
        Some(ModelTier::Weak)
    );
    assert_eq!(
        parsed_tier(r#"{"route":"weak","reason":"single command"}"#),
        Some(ModelTier::Weak)
    );
    assert_eq!(
        parsed_tier(r#"{"route":"medium","reason":"localized edit"}"#),
        Some(ModelTier::Medium)
    );
}

#[test]
fn parse_judge_reply_handles_code_fence() {
    let raw = "```json\n{\"route\":\"parent\",\"reason\":\"needs reasoning\"}\n```";
    assert_eq!(parsed_tier(raw), Some(ModelTier::Strong));
    let strong = "```json\n{\"route\":\"strong\",\"reason\":\"needs reasoning\"}\n```";
    assert_eq!(parsed_tier(strong), Some(ModelTier::Strong));
}

#[test]
fn parse_judge_reply_extracts_per_task_effort_when_present() {
    let verdict =
        super::parse_judge_reply(r#"{"route":"strong","effort":"xhigh","reason":"tricky"}"#)
            .expect("parses");
    assert_eq!(verdict.tier, ModelTier::Strong);
    assert_eq!(verdict.effort, Some(squeezy_core::ReasoningEffort::XHigh));
    // Absent or unparseable effort just leaves it unset.
    let no_effort = super::parse_judge_reply(r#"{"route":"weak","reason":"x"}"#).expect("parses");
    assert_eq!(no_effort.effort, None);
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

// -- Structured-output contract (M13) --------------------------------------

/// The judge schema must mirror exactly what `JudgeReply` deserializes:
/// the two `route` values `parse_judge_reply` accepts plus a `reason`
/// string. A document that validates against the schema must also
/// deserialize into `JudgeReply` and route correctly, and the schema must
/// be marked strict so supporting providers enforce it server-side.
#[test]
fn judge_output_schema_mirrors_judge_reply() {
    let schema = super::judge_output_schema(false);
    assert!(schema.strict, "judge schema must be strict");

    let props = &schema.schema["properties"];
    let route_enum = props["route"]["enum"]
        .as_array()
        .expect("route carries an enum");
    let values: Vec<&str> = route_enum.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(
        values,
        vec!["weak", "medium", "strong"],
        "route enum is the canonical tier set"
    );
    assert_eq!(props["reason"]["type"], "string");
    assert_eq!(
        schema.schema["required"],
        serde_json::json!(["route", "reason"])
    );
    assert_eq!(
        schema.schema["additionalProperties"],
        serde_json::json!(false)
    );

    // A schema-valid document deserializes into the real parse target and
    // routes to the expected tier — the schema cannot drift from the struct
    // without this failing.
    for (route, expect) in [
        ("weak", Some(ModelTier::Weak)),
        ("medium", Some(ModelTier::Medium)),
        ("strong", Some(ModelTier::Strong)),
    ] {
        let doc = serde_json::json!({ "route": route, "reason": "x" });
        let reply: super::JudgeReply = serde_json::from_value(doc.clone()).expect("deserializes");
        assert_eq!(reply.route, route);
        assert_eq!(parsed_tier(&doc.to_string()), expect);
    }
    // The loose parser still accepts the legacy binary vocabulary so older judge
    // prompts (and the scripted integration provider) keep routing correctly.
    assert_eq!(
        parsed_tier(r#"{"route":"cheap","reason":"x"}"#),
        Some(ModelTier::Weak)
    );
    assert_eq!(
        parsed_tier(r#"{"route":"parent","reason":"x"}"#),
        Some(ModelTier::Strong)
    );

    // With judge-effort on, the schema additionally requires an effort enum.
    let effort_schema = super::judge_output_schema(true);
    let effort_values: Vec<&str> = effort_schema.schema["properties"]["effort"]["enum"]
        .as_array()
        .expect("effort carries an enum")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(effort_values, vec!["low", "medium", "high", "xhigh"]);
    assert_eq!(
        effort_schema.schema["required"],
        serde_json::json!(["route", "effort", "reason"])
    );
}

/// Records every request it is handed so a test can inspect the
/// `output_schema` the judge attaches. Emits a single valid judge reply so
/// `run_judge` runs to completion.
struct RecordingProvider {
    requests: std::sync::Mutex<Vec<squeezy_llm::LlmRequest>>,
}

impl RecordingProvider {
    fn new() -> Self {
        Self {
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn last_request(&self) -> squeezy_llm::LlmRequest {
        self.requests
            .lock()
            .expect("requests")
            .last()
            .cloned()
            .expect("a request was recorded")
    }
}

impl squeezy_llm::LlmProvider for RecordingProvider {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn stream_response(
        &self,
        request: squeezy_llm::LlmRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> squeezy_llm::LlmStream {
        self.requests.lock().expect("requests").push(request);
        let events = vec![
            Ok(squeezy_llm::LlmEvent::TextDelta(
                r#"{"route":"cheap","reason":"single op"}"#.to_string(),
            )),
            Ok(squeezy_llm::LlmEvent::Completed {
                cost: CostSnapshot::default(),
                response_id: None,
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ];
        Box::pin(futures_util::stream::iter(events))
    }
}

/// On a provider that forwards `output_schema` (OpenAI), the judge request
/// carries the strict `JudgeReply` schema; on a provider that drops it
/// (Anthropic) the request stays `None` so the loose parser remains the
/// contract and behavior is unchanged.
#[tokio::test]
async fn run_judge_attaches_schema_only_on_supporting_provider() {
    use std::sync::Arc;

    let recorder = Arc::new(RecordingProvider::new());
    let provider: Arc<dyn squeezy_llm::LlmProvider> = recorder.clone();

    // Supporting provider: schema present and identical to the builder.
    let model: Arc<str> = Arc::from("gpt-5.5");
    let (verdict, _) = super::run_judge(
        &provider,
        "openai",
        &model,
        "judge instructions",
        "rename foo to bar in src/lib.rs",
        false,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert_eq!(verdict.map(|v| v.tier), Some(ModelTier::Weak));
    let req = recorder.last_request();
    assert_eq!(
        req.output_schema,
        Some(super::judge_output_schema(false)),
        "openai must carry the judge schema"
    );

    // Non-supporting provider: no schema, unchanged behavior.
    let model: Arc<str> = Arc::from("claude-opus-4-7");
    let _ = super::run_judge(
        &provider,
        "anthropic",
        &model,
        "judge instructions",
        "rename foo to bar in src/lib.rs",
        false,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    let req = recorder.last_request();
    assert_eq!(
        req.output_schema, None,
        "anthropic drops output_schema; request stays None"
    );
}

#[test]
fn deictic_followup_detector_matches_short_followups() {
    assert!(super::is_deictic_followup(
        "now do the same for the websocket client"
    ));
    assert!(super::is_deictic_followup("keep going"));
    assert!(super::is_deictic_followup("continue"));
    assert!(!super::is_deictic_followup("run cargo test"));
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
    assert_eq!(super::count_sentences("run test.Then commit.Then push"), 3);
    assert_eq!(super::count_sentences("run e.g. cargo test"), 1);
    assert_eq!(super::count_sentences("run i.e. only the focused test"), 1);
    assert_eq!(super::count_sentences("run cargo test etc. Then push."), 2);
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
            prompt.contains("'weak'") && prompt.contains("'medium'") && prompt.contains("'strong'"),
            "{provider} variant must carry the weak/medium/strong guidance: {prompt}"
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
    let net = estimate_routing_net_savings("anthropic", "claude-opus-4-7", &cheap_cost, 0);
    assert!(net < 0);
}

#[test]
fn estimate_routing_net_savings_subtracts_judge_cost() {
    let cheap_cost = cost_with(10_000, 1_000);
    let gross = estimate_routing_net_savings("anthropic", "claude-opus-4-7", &cheap_cost, 0);
    assert!(gross > 100);

    let net = estimate_routing_net_savings("anthropic", "claude-opus-4-7", &cheap_cost, 100);

    assert_eq!(net, gross - 100);
}

// -- Per-provider resolution -----------------------------------------------

fn app_config_with_providers(
    providers: std::collections::BTreeMap<String, squeezy_core::ProviderSettings>,
) -> squeezy_core::AppConfig {
    squeezy_core::AppConfig {
        providers,
        ..squeezy_core::AppConfig::default()
    }
}

fn provider_settings(reroute: Option<&str>, judge: Option<&str>) -> squeezy_core::ProviderSettings {
    squeezy_core::ProviderSettings {
        cheap_model: reroute.map(str::to_string),
        judge_model: judge.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn cheap_model_resolves_per_provider_and_never_crosses() {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "openai".to_string(),
        provider_settings(Some("my-openai-cheap"), None),
    );
    let cfg = app_config_with_providers(providers);

    // The active provider's override wins.
    assert_eq!(
        crate::cheap_model_for("openai", &cfg).as_deref(),
        Some("my-openai-cheap")
    );
    // A different provider does NOT inherit openai's choice — it falls back to
    // its own built-in cheap tier.
    assert_eq!(
        crate::cheap_model_for("anthropic", &cfg).as_deref(),
        Some("claude-haiku-4-5-20251001")
    );
}

#[test]
fn judge_model_defaults_to_per_provider_mini() {
    let cfg = app_config_with_providers(std::collections::BTreeMap::new());
    let cheap: std::sync::Arc<str> = std::sync::Arc::from("gpt-5.4-nano");
    // OpenAI default judge is the mini tier, not the cheaper nano.
    assert_eq!(
        &*super::judge_model_for("openai", &cfg, &cheap),
        "gpt-5.4-mini"
    );
    // Anthropic's mini == its small tier (haiku).
    let cheap_a: std::sync::Arc<str> = std::sync::Arc::from("claude-haiku-4-5-20251001");
    assert_eq!(
        &*super::judge_model_for("anthropic", &cfg, &cheap_a),
        "claude-haiku-4-5-20251001"
    );
}

#[test]
fn judge_model_per_provider_override_wins() {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "openai".to_string(),
        provider_settings(None, Some("my-judge")),
    );
    let cfg = app_config_with_providers(providers);
    let cheap: std::sync::Arc<str> = std::sync::Arc::from("gpt-5.4-nano");
    assert_eq!(&*super::judge_model_for("openai", &cfg, &cheap), "my-judge");
}

// -- Linux sandbox-sensitive routing ----------------------------------------

#[test]
fn linux_sandbox_sensitive_detects_known_keywords() {
    assert!(super::is_linux_sandbox_sensitive("run unshare -r sh"));
    assert!(super::is_linux_sandbox_sensitive(
        "check if landlock is available"
    ));
    assert!(super::is_linux_sandbox_sensitive("install with apt-get"));
    assert!(super::is_linux_sandbox_sensitive("run docker build ."));
    assert!(super::is_linux_sandbox_sensitive("try sudo sysctl"));
    assert!(super::is_linux_sandbox_sensitive(
        "inspect /proc/self/status"
    ));
    assert!(super::is_linux_sandbox_sensitive("read /sys/kernel/debug"));
    assert!(super::is_linux_sandbox_sensitive("run podman run --rm"));
    assert!(super::is_linux_sandbox_sensitive(
        "configure seccomp policy"
    ));
    assert!(super::is_linux_sandbox_sensitive(
        "check user namespace support"
    ));
}

#[test]
fn linux_sandbox_sensitive_case_insensitive() {
    assert!(super::is_linux_sandbox_sensitive("Run Docker Build ."));
    assert!(super::is_linux_sandbox_sensitive("SUDO systemctl restart"));
    assert!(super::is_linux_sandbox_sensitive("check SECCOMP filter"));
}

#[test]
fn linux_sandbox_sensitive_does_not_match_unrelated_prompts() {
    assert!(!super::is_linux_sandbox_sensitive("run cargo test"));
    assert!(!super::is_linux_sandbox_sensitive("ls src/lib.rs"));
    assert!(!super::is_linux_sandbox_sensitive("checkout main branch"));
    assert!(!super::is_linux_sandbox_sensitive("grep TODO src/"));
    assert!(!super::is_linux_sandbox_sensitive("format the code"));
}

// -- Cache-isolation gate ---------------------------------------------------

#[test]
fn should_isolate_truth_table() {
    use squeezy_core::CacheIsolation;
    let mut cfg = default_routing_config();

    cfg.cache_isolation = CacheIsolation::Switch;
    assert!(
        !super::should_isolate(&cfg, 1_000_000, true),
        "switch never isolates"
    );

    cfg.cache_isolation = CacheIsolation::Subagent;
    assert!(
        super::should_isolate(&cfg, 0, false),
        "subagent always isolates"
    );

    cfg.cache_isolation = CacheIsolation::Auto;
    cfg.auto_prefix_token_threshold = 8_000;
    assert!(
        !super::should_isolate(&cfg, 8_000, true),
        "auto: at threshold is not over it"
    );
    assert!(
        super::should_isolate(&cfg, 8_001, true),
        "auto: over threshold with caching isolates"
    );
    assert!(
        !super::should_isolate(&cfg, 1_000_000, false),
        "auto: no caching support → no isolation"
    );
}
