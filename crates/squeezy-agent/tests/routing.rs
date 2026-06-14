//! End-to-end coverage for the per-turn model router.
//!
//! Routing has a tight algorithmic core that lives in
//! `crates/squeezy-agent/src/turn_router.rs` and is heavily unit-tested
//! in `turn_router_tests.rs`. This file pins the dispatch wiring:
//! `classify_turn` → `LlmRequest::model` selection → `AgentEvent::TurnRouted`
//! emission → mid-turn escalation back to parent. The scripted
//! provider here returns canned `LlmEvent` sequences keyed on the
//! request order so we can assert which model each request landed on.

use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_core::Stream;
use futures_util::stream;
use squeezy_agent::{Agent, AgentEvent};
use squeezy_core::{
    AppConfig, CostSnapshot, PermissionMode, PermissionPolicy, Result, SessionMode, SqueezyError,
};
use squeezy_llm::{LlmEvent, LlmProvider, LlmRequest, LlmStream};
use tokio_util::sync::CancellationToken;

const PARENT_MODEL: &str = "claude-opus-4-7";
const CHEAP_MODEL: &str = "claude-haiku-4-5-20251001";
const MEDIUM_MODEL: &str = "claude-sonnet-4-6";

fn cheap_judge_completed_event() -> LlmEvent {
    LlmEvent::Completed {
        response_id: None,
        cost: CostSnapshot::default(),
        stop_reason: None,
        reasoning_only_stop: false,
    }
}

fn end_turn_completed_event() -> LlmEvent {
    LlmEvent::Completed {
        response_id: None,
        cost: CostSnapshot::default(),
        stop_reason: Some(squeezy_llm::StopReason::EndTurn),
        reasoning_only_stop: false,
    }
}

fn judge_reply(verdict: &str) -> Vec<Result<LlmEvent>> {
    vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta(format!(
            "{{\"route\":\"{verdict}\",\"reason\":\"test\"}}"
        ))),
        Ok(cheap_judge_completed_event()),
    ]
}

fn judge_reply_with_effort(verdict: &str, effort: &str) -> Vec<Result<LlmEvent>> {
    vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta(format!(
            "{{\"route\":\"{verdict}\",\"effort\":\"{effort}\",\"reason\":\"test\"}}"
        ))),
        Ok(cheap_judge_completed_event()),
    ]
}

fn end_turn_reply(text: &str) -> Vec<Result<LlmEvent>> {
    vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta(text.to_string())),
        Ok(end_turn_completed_event()),
    ]
}

/// A canned provider that pops one scripted `Vec<LlmEvent>` per call.
/// Provider name is `"anthropic"` so `cheap_model_for(...)` resolves to
/// Haiku and the router has a real cheap tier to target.
struct ScriptedProvider {
    responses: Mutex<VecDeque<Vec<Result<LlmEvent>>>>,
    requests: Mutex<Vec<LlmRequest>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<Vec<Result<LlmEvent>>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<LlmRequest> {
        self.requests.lock().expect("requests").clone()
    }
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn stream_response(&self, request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        self.requests.lock().expect("requests").push(request);
        let events = self
            .responses
            .lock()
            .expect("responses")
            .pop_front()
            .expect("scripted response queue exhausted");
        let stream: Pin<Box<dyn Stream<Item = Result<LlmEvent>> + Send>> =
            Box::pin(stream::iter(events));
        stream
    }
}

fn temp_workspace(name: &str) -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy_routing_test_{name}_{nonce}"));
    std::fs::create_dir_all(&root).expect("create temp workspace");
    root
}

fn config_with_routing() -> AppConfig {
    let root = temp_workspace("routing");
    let mut config = AppConfig {
        workspace_root: root,
        permissions: PermissionPolicy {
            edit: PermissionMode::Allow,
            ..Default::default()
        },
        ..Default::default()
    };
    config.model = PARENT_MODEL.to_string();
    // Routing is opt-out in production; the integration tests opt in
    // explicitly because they want to exercise the routed path.
    config.routing.enabled = true;
    config.routing.llm_judge = true;
    config.routing.escalation_sticky_turns = 3;
    // Tighten the escalation thresholds so the scripted refusal triggers
    // the handoff without having to script a long stream.
    config.routing.cheap_escalation_error_threshold = 1;
    config
}

async fn drain_until_terminal(mut rx: tokio::sync::mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        let is_terminal = matches!(
            event,
            AgentEvent::Completed { .. } | AgentEvent::Failed { .. } | AgentEvent::Cancelled { .. }
        );
        events.push(event);
        if is_terminal {
            break;
        }
    }
    events
}

#[tokio::test]
async fn heuristic_slam_dunk_dispatches_on_cheap_model() {
    // The heuristic matches "checkout main" directly so the router skips
    // the judge call. The provider sees exactly one request (the routed
    // turn) and replies with a clean end-turn.
    let provider = Arc::new(ScriptedProvider::new(vec![end_turn_reply("ok, on it.")]));
    let agent = Agent::new(config_with_routing(), provider.clone());
    let events = drain_until_terminal(
        agent.start_turn("checkout main".to_string(), CancellationToken::new()),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1, "heuristic must not call the judge");
    assert_eq!(&*requests[0].model, CHEAP_MODEL);

    let routed = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::TurnRouted {
                from, to, reason, ..
            } => Some((from.clone(), to.clone(), reason.clone())),
            _ => None,
        })
        .expect("must emit TurnRouted on a routed turn");
    assert_eq!(routed.0, PARENT_MODEL);
    assert_eq!(routed.1, CHEAP_MODEL);
    assert_eq!(routed.2, "checkout");
}

#[tokio::test]
async fn cheap_model_override_alias_resolves_before_dispatch() {
    let provider = Arc::new(ScriptedProvider::new(vec![end_turn_reply("ok, on it.")]));
    let mut config = config_with_routing();
    config.small_fast_model = Some("haiku".to_string());
    let agent = Agent::new(config, provider.clone());
    let _events = drain_until_terminal(
        agent.start_turn("checkout main".to_string(), CancellationToken::new()),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(&*requests[0].model, CHEAP_MODEL);
}

#[tokio::test]
async fn plan_mode_turns_stay_on_parent_model() {
    let provider = Arc::new(ScriptedProvider::new(vec![end_turn_reply("plan")]));
    let mut config = config_with_routing();
    config.session_mode = SessionMode::Plan;
    let agent = Agent::new(config, provider.clone());
    let events = drain_until_terminal(
        agent.start_turn("checkout main".to_string(), CancellationToken::new()),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(&*requests[0].model, PARENT_MODEL);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnRouted { .. })),
        "plan-mode parent bias must not emit cheap-route event"
    );
}

#[tokio::test]
async fn llm_judge_cheap_verdict_routes_borderline_prompt() {
    // The prompt does not match the heuristic ("explain how...") so the
    // router calls the judge. Judge votes "cheap" so the next request is
    // the actual turn dispatched on the cheap model.
    let provider = Arc::new(ScriptedProvider::new(vec![
        judge_reply("cheap"),
        end_turn_reply("here you go"),
    ]));
    let agent = Agent::new(config_with_routing(), provider.clone());
    let events = drain_until_terminal(agent.start_turn(
        "explain how the cost broker tracks budgets".to_string(),
        CancellationToken::new(),
    ))
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "judge + turn must produce two requests");
    assert_eq!(
        &*requests[0].model, CHEAP_MODEL,
        "judge dispatches on the cheap tier"
    );
    assert_eq!(
        &*requests[1].model, CHEAP_MODEL,
        "routed turn also dispatches on the cheap tier"
    );
    assert!(
        requests[0].cache.key.is_none(),
        "short judge prompt must not request provider prompt caching"
    );
    assert_eq!(requests[0].max_output_tokens, Some(512));
    assert_eq!(requests[0].reasoning_effort, None);

    let reason = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::TurnRouted { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .expect("must emit TurnRouted with judge reason");
    assert_eq!(reason, "llm_judge");
}

#[tokio::test]
async fn llm_judge_medium_verdict_routes_to_mid_tier() {
    // The Anthropic ladder for an Opus parent is Haiku → Sonnet → Opus. A
    // "medium" verdict routes the turn to the Sonnet mid rung — the rung that
    // simply did not exist in the old binary cheap↔parent router.
    let provider = Arc::new(ScriptedProvider::new(vec![
        judge_reply("medium"),
        end_turn_reply("handled on sonnet"),
    ]));
    let agent = Agent::new(config_with_routing(), provider.clone());
    let events = drain_until_terminal(agent.start_turn(
        "add a focused null-check to the parser in src/parse.rs".to_string(),
        CancellationToken::new(),
    ))
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "judge + turn must produce two requests");
    assert_eq!(
        &*requests[0].model, CHEAP_MODEL,
        "judge dispatches on the cheap tier"
    );
    assert_eq!(
        &*requests[1].model, MEDIUM_MODEL,
        "medium verdict routes the turn to the Sonnet mid rung"
    );

    let routed = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::TurnRouted { to, reason, .. } if reason == "llm_judge" => Some(to.clone()),
            _ => None,
        })
        .expect("medium route must emit TurnRouted with the judge reason");
    assert_eq!(routed, MEDIUM_MODEL);
}

#[tokio::test]
async fn configured_judge_model_dispatches_judge_only() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        judge_reply("cheap"),
        end_turn_reply("here you go"),
    ]));
    let mut config = config_with_routing();
    config.routing.judge_model = Some("sonnet".to_string());
    let agent = Agent::new(config, provider.clone());
    let _events = drain_until_terminal(agent.start_turn(
        "explain how the cost broker tracks budgets".to_string(),
        CancellationToken::new(),
    ))
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(&*requests[0].model, "claude-sonnet-4-6");
    assert_eq!(&*requests[1].model, CHEAP_MODEL);
}

#[tokio::test]
async fn llm_judge_parent_verdict_skips_routing() {
    // Judge votes "parent"; the actual turn dispatches on the parent
    // model and no `TurnRouted` event is emitted.
    let provider = Arc::new(ScriptedProvider::new(vec![
        judge_reply("parent"),
        end_turn_reply("still on opus"),
    ]));
    let agent = Agent::new(config_with_routing(), provider.clone());
    let events = drain_until_terminal(agent.start_turn(
        "refactor the dispatch layer across crates".to_string(),
        CancellationToken::new(),
    ))
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(&*requests[0].model, CHEAP_MODEL, "judge runs on cheap tier");
    assert_eq!(
        &*requests[1].model, PARENT_MODEL,
        "parent verdict keeps the turn on parent"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnRouted { .. })),
        "parent verdict must not emit a routing event"
    );
}

// NOTE: mid-stream escalation coverage (e.g. cheap model emits "I'm
// not sure" and the swap fires before the next round's LlmRequest is
// built) requires either a multi-round scripted turn with a tool call
// landing the escalation at the round boundary, or PR-D's
// `TextDelta`-level escalation polling. Once PR-D lands, this file
// gains a test that streams `"thinking…I'm not sure"` and asserts the
// swap surfaces before `Completed`. Today's escalation pathway is
// already pinned by the `escalation_*` unit tests in
// `turn_router_tests.rs`.

#[tokio::test]
async fn judge_timeout_falls_through_to_parent() {
    // Empty response queue causes the scripted provider to panic on a
    // second `stream_response` call. We rely on the judge timeout (the
    // judge stream never reaches `Completed`) to short-circuit before
    // the second pop. The scripted provider yields nothing for the
    // judge: an immediate `Started` then nothing — the stream ends but
    // never emits `Completed`, so `run_judge` waits until its 10s
    // budget elapses. For test-speed we use a fast timeout via a small
    // judge_max_chars=1 instead, so the prompt is too long for the
    // judge to even fire and routing defers to parent directly without
    // any second LLM call.
    let provider = Arc::new(ScriptedProvider::new(vec![end_turn_reply("default")]));
    let mut config = config_with_routing();
    config.routing.judge_max_chars = 1; // forces the judge to skip
    let agent = Agent::new(config, provider.clone());
    let events = drain_until_terminal(agent.start_turn(
        "explain how the cost broker tracks budgets".to_string(),
        CancellationToken::new(),
    ))
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1, "judge must not be invoked");
    assert_eq!(
        &*requests[0].model, PARENT_MODEL,
        "long-prompt path must dispatch on parent"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnRouted { .. })),
        "no routing event on the parent fallback"
    );
}

#[tokio::test]
async fn force_cheap_override_dispatches_on_cheap_tier() {
    // `Agent::request_routing_force_cheap` is the slash-command API
    // hook. Triggering it before `start_turn` must route the next turn
    // on cheap even when the heuristic would not have fired.
    let provider = Arc::new(ScriptedProvider::new(vec![end_turn_reply("forced")]));
    let agent = Agent::new(config_with_routing(), provider.clone());
    agent.request_routing_force_cheap();
    let events = drain_until_terminal(agent.start_turn(
        "explain the routing classifier in detail".to_string(),
        CancellationToken::new(),
    ))
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1, "force_cheap skips the judge call");
    assert_eq!(&*requests[0].model, CHEAP_MODEL);

    let reason = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::TurnRouted { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .expect("explicit override must emit TurnRouted");
    assert_eq!(reason, "user_explicit");
}

#[tokio::test]
async fn force_cheap_override_wins_inside_sticky_window() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        end_turn_reply("I'm not sure how to proceed."),
        end_turn_reply("forced cheap"),
    ]));
    let agent = Agent::new(config_with_routing(), provider.clone());
    let _first = drain_until_terminal(
        agent.start_turn("checkout main".to_string(), CancellationToken::new()),
    )
    .await;

    agent.request_routing_force_cheap();
    let events = drain_until_terminal(agent.start_turn(
        "explain the routing classifier in detail".to_string(),
        CancellationToken::new(),
    ))
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(&*requests[0].model, CHEAP_MODEL);
    assert_eq!(&*requests[1].model, CHEAP_MODEL);
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::TurnRouted { reason, .. } if reason == "user_explicit"
        )),
        "explicit cheap override must not be consumed by sticky parent routing"
    );
}

#[tokio::test]
async fn force_cheap_override_does_not_bypass_plan_mode() {
    let provider = Arc::new(ScriptedProvider::new(vec![end_turn_reply("plan")]));
    let mut config = config_with_routing();
    config.session_mode = SessionMode::Plan;
    let agent = Agent::new(config, provider.clone());
    agent.request_routing_force_cheap();
    let events = drain_until_terminal(
        agent.start_turn("checkout main".to_string(), CancellationToken::new()),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(&*requests[0].model, PARENT_MODEL);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnRouted { .. })),
        "plan mode remains a hard parent-model gate"
    );
}

#[tokio::test]
async fn force_cheap_override_does_not_bypass_large_attachment() {
    let provider = Arc::new(ScriptedProvider::new(vec![end_turn_reply("parent")]));
    let mut config = config_with_routing();
    config.routing.large_attachment_bypass_bytes = 1;
    let agent = Agent::new(config, provider.clone());
    agent
        .attach_pasted_context("large pasted context".to_string())
        .await
        .expect("attach context");
    agent.request_routing_force_cheap();
    let events = drain_until_terminal(
        agent.start_turn("checkout main".to_string(), CancellationToken::new()),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(&*requests[0].model, PARENT_MODEL);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnRouted { .. })),
        "large pasted context remains a hard parent-model gate"
    );
}

#[tokio::test]
async fn session_disabled_blocks_implicit_routing() {
    // `set_routing_session_disabled(true)` mirrors the `/router off`
    // command. The slam-dunk prompt is still routed on parent because
    // the session-wide toggle takes precedence over the heuristic.
    let provider = Arc::new(ScriptedProvider::new(vec![end_turn_reply("parent only")]));
    let agent = Agent::new(config_with_routing(), provider.clone());
    agent.set_routing_session_disabled(true);
    let events = drain_until_terminal(
        agent.start_turn("checkout main".to_string(), CancellationToken::new()),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(&*requests[0].model, PARENT_MODEL);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnRouted { .. })),
        "session toggle off must suppress implicit routing"
    );
}

#[tokio::test]
async fn cheap_provider_error_retries_once_on_parent() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![Err(SqueezyError::ProviderStream(
            "cheap model not found".to_string(),
        ))],
        end_turn_reply("parent recovered"),
    ]));
    let agent = Agent::new(config_with_routing(), provider.clone());
    let events = drain_until_terminal(
        agent.start_turn("checkout main".to_string(), CancellationToken::new()),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(&*requests[0].model, CHEAP_MODEL);
    assert_eq!(&*requests[1].model, PARENT_MODEL);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Completed { .. })),
        "parent retry should complete the turn"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::TurnRouted { reason, .. }
                if reason == "escalated_provider_error"
        )),
        "provider error must emit an escalation routing event"
    );
}

#[tokio::test]
async fn tier_effort_runs_weak_rung_at_low_effort() {
    // With tier_effort on (default) and no user pin, a weak-routed turn runs at
    // low reasoning effort — effort tracks the rung, not one global value.
    let provider = Arc::new(ScriptedProvider::new(vec![end_turn_reply("ok")]));
    let agent = Agent::new(config_with_routing(), provider.clone());
    let _ = drain_until_terminal(
        agent.start_turn("checkout main".to_string(), CancellationToken::new()),
    )
    .await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(&*requests[0].model, CHEAP_MODEL);
    assert_eq!(
        requests[0].reasoning_effort,
        Some(squeezy_core::ReasoningEffort::Low),
        "weak rung runs at low effort under tier_effort"
    );
}

#[tokio::test]
async fn explicit_effort_pin_overrides_tier_effort() {
    // A user /effort pin is a hard override on every rung.
    let provider = Arc::new(ScriptedProvider::new(vec![end_turn_reply("ok")]));
    let mut config = config_with_routing();
    config.reasoning_effort = Some(squeezy_core::ReasoningEffort::High);
    let agent = Agent::new(config, provider.clone());
    let _ = drain_until_terminal(
        agent.start_turn("checkout main".to_string(), CancellationToken::new()),
    )
    .await;
    let requests = provider.requests();
    assert_eq!(
        requests[0].reasoning_effort,
        Some(squeezy_core::ReasoningEffort::High),
        "an explicit effort pin wins over the rung default"
    );
}

#[tokio::test]
async fn judge_effort_sets_per_task_effort_on_the_parent_rung() {
    // The user's case: two tasks both land on the strong/parent rung but want
    // different depths. With judge_effort on, the judge's per-task effort drives
    // the wire effort — here "strong"+"xhigh" runs the parent turn at xhigh.
    let provider = Arc::new(ScriptedProvider::new(vec![
        judge_reply_with_effort("strong", "xhigh"),
        end_turn_reply("deep work"),
    ]));
    let mut config = config_with_routing();
    config.routing.judge_effort = true;
    let agent = Agent::new(config, provider.clone());
    let _ = drain_until_terminal(agent.start_turn(
        "investigate the intermittent deadlock in the scheduler".to_string(),
        CancellationToken::new(),
    ))
    .await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "judge + parent turn");
    assert_eq!(
        &*requests[1].model, PARENT_MODEL,
        "strong verdict stays on parent"
    );
    assert_eq!(
        requests[1].reasoning_effort,
        Some(squeezy_core::ReasoningEffort::XHigh),
        "judge's per-task effort drives the parent turn's depth"
    );
}

#[tokio::test]
async fn judge_effort_off_ignores_judge_effort_field() {
    // Same judge reply, but judge_effort off: the effort field is ignored and the
    // parent rung keeps the provider default (no effort on the wire).
    let provider = Arc::new(ScriptedProvider::new(vec![
        judge_reply_with_effort("strong", "xhigh"),
        end_turn_reply("default depth"),
    ]));
    let config = config_with_routing(); // judge_effort defaults off
    let agent = Agent::new(config, provider.clone());
    let _ = drain_until_terminal(agent.start_turn(
        "investigate the intermittent deadlock in the scheduler".to_string(),
        CancellationToken::new(),
    ))
    .await;
    let requests = provider.requests();
    assert_eq!(&*requests[1].model, PARENT_MODEL);
    assert_eq!(
        requests[1].reasoning_effort, None,
        "judge_effort off leaves the parent rung at the provider default"
    );
}

#[tokio::test]
async fn tier_effort_off_keeps_provider_default_effort() {
    // tier_effort=false restores the pre-feature behavior: no effort field
    // (provider default) when the user hasn't pinned one.
    let provider = Arc::new(ScriptedProvider::new(vec![end_turn_reply("ok")]));
    let mut config = config_with_routing();
    config.routing.tier_effort = false;
    let agent = Agent::new(config, provider.clone());
    let _ = drain_until_terminal(
        agent.start_turn("checkout main".to_string(), CancellationToken::new()),
    )
    .await;
    let requests = provider.requests();
    assert_eq!(
        requests[0].reasoning_effort, None,
        "tier_effort off sends no effort when unpinned"
    );
}

// Guard against the test runtime hanging if the agent task gets stuck
// in a route-then-cancel ping-pong. 30s is well above the agent's own
// classification / judge / dispatch budget on any reasonable host.
fn _enforce_test_timeout() -> Duration {
    Duration::from_secs(30)
}

// -- Linux sandbox-sensitive routing ----------------------------------------

#[tokio::test]
async fn linux_sandbox_sensitive_prompt_stays_on_parent() {
    // A prompt that would normally slam-dunk to cheap ("run docker build .")
    // must stay on parent when linux_sandbox_sensitive_parent is true (the default).
    let provider = Arc::new(ScriptedProvider::new(vec![end_turn_reply("done")]));
    let mut config = config_with_routing();
    config.routing.linux_sandbox_sensitive_parent = true;
    config.routing.heuristic = true;
    let agent = Agent::new(config, provider.clone());
    let events = drain_until_terminal(
        agent.start_turn("run docker build .".to_string(), CancellationToken::new()),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        1,
        "linux-sensitive prompt must not call judge"
    );
    assert_eq!(
        &*requests[0].model, PARENT_MODEL,
        "linux-sensitive prompt must stay on parent model"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnRouted { .. })),
        "linux-sensitive parent bias must not emit cheap-route event"
    );
}

#[tokio::test]
async fn linux_sandbox_sensitive_guard_disabled_allows_cheap_route() {
    // When linux_sandbox_sensitive_parent = false, a Docker prompt falls
    // through to the heuristic which can still cheap-route it.
    let provider = Arc::new(ScriptedProvider::new(vec![end_turn_reply("done")]));
    let mut config = config_with_routing();
    config.routing.linux_sandbox_sensitive_parent = false;
    config.routing.heuristic = true;
    config.routing.llm_judge = false;
    let agent = Agent::new(config, provider.clone());
    let _events = drain_until_terminal(
        agent.start_turn("run docker build .".to_string(), CancellationToken::new()),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    // With the guard off, "run" slam-dunks to cheap
    assert_eq!(
        &*requests[0].model, CHEAP_MODEL,
        "disabled linux guard must allow heuristic cheap route for 'run' verb"
    );
}

#[tokio::test]
async fn explicit_cheap_override_beats_linux_sandbox_guard() {
    // An explicit `/cheap` flag must win even over the linux sandbox guard.
    let provider = Arc::new(ScriptedProvider::new(vec![end_turn_reply("done")]));
    let mut config = config_with_routing();
    config.routing.linux_sandbox_sensitive_parent = true;
    config.routing.heuristic = true;
    let agent = Agent::new(config, provider.clone());
    agent.request_routing_force_cheap();
    let _events = drain_until_terminal(
        agent.start_turn("run docker build .".to_string(), CancellationToken::new()),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        &*requests[0].model, CHEAP_MODEL,
        "/cheap override must beat the linux sandbox guard"
    );
}
