use super::*;
use serde_json::json;

/// A no-op handler that records every event it sees and returns
/// `allow=true` with no mutation or message.
struct NoopHandler;

impl HookHandler for NoopHandler {
    fn handle(&self, _ctx: &HookContext) -> HookResult {
        HookResult::allow()
    }
}

/// A handler that proposes a mutation. Used to verify that the
/// dispatch contract propagates `mutate` back to the caller even
/// though most legacy sites still treat the proposed mutation as
/// advisory.
struct MutatingHandler {
    replacement: Value,
}

impl HookHandler for MutatingHandler {
    fn handle(&self, _ctx: &HookContext) -> HookResult {
        HookResult {
            allow: true,
            mutate: Some(self.replacement.clone()),
            message: None,
        }
    }
}

#[test]
fn dispatch_preturn_with_noop_handler_returns_single_allow() {
    let mut registry = HookRegistry::new();
    registry.register(Box::new(NoopHandler));

    let results = registry.dispatch(HookPayload::PreTurn {
        turn_id: "0".to_string(),
    });

    assert_eq!(results.len(), 1);
    assert!(results[0].allow);
    assert!(results[0].mutate.is_none());
    assert!(results[0].message.is_none());
}

#[test]
fn empty_registry_dispatches_to_no_handlers() {
    let registry = HookRegistry::new();
    let results = registry.dispatch(HookPayload::PreTurn {
        turn_id: "0".to_string(),
    });
    assert!(results.is_empty());
    assert!(registry.is_empty());
}

#[test]
fn dispatch_no_collect_invokes_handlers_without_returning_results() {
    struct CountingHandler {
        calls: std::sync::Arc<std::sync::Mutex<u32>>,
    }

    impl HookHandler for CountingHandler {
        fn handle(&self, _ctx: &HookContext) -> HookResult {
            *self.calls.lock().unwrap() += 1;
            HookResult::deny("ignored at observation-only sites")
        }
    }

    let calls = std::sync::Arc::new(std::sync::Mutex::new(0));
    let mut registry = HookRegistry::new();
    registry.register(Box::new(CountingHandler {
        calls: calls.clone(),
    }));

    registry.dispatch_no_collect(HookPayload::PreTurn {
        turn_id: "0".to_string(),
    });

    assert_eq!(*calls.lock().unwrap(), 1);
}

#[test]
fn handlers_can_propose_mutations_visible_to_callers() {
    let mut registry = HookRegistry::new();
    registry.register(Box::new(MutatingHandler {
        replacement: json!({ "extra_instructions": "Be terse." }),
    }));

    let results = registry.dispatch(HookPayload::PreTurn {
        turn_id: "1".to_string(),
    });
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].mutate.as_ref().unwrap(),
        &json!({ "extra_instructions": "Be terse." })
    );
}

#[test]
fn deny_constructor_carries_message_and_blocks() {
    let result = HookResult::deny("policy violation");
    assert!(!result.allow);
    assert_eq!(result.message.as_deref(), Some("policy violation"));
}

#[test]
fn dispatch_context_preserves_event_and_payload() {
    /// Captures the last context the handler saw so tests can verify
    /// the registry forwarded both the event and payload faithfully.
    struct Recorder {
        seen: std::sync::Mutex<Option<HookContext>>,
    }

    impl HookHandler for Recorder {
        fn handle(&self, ctx: &HookContext) -> HookResult {
            *self.seen.lock().unwrap() = Some(ctx.clone());
            HookResult::allow()
        }
    }

    let recorder = Recorder {
        seen: std::sync::Mutex::new(None),
    };
    let recorder = std::sync::Arc::new(recorder);
    struct RecorderRef(std::sync::Arc<Recorder>);
    impl HookHandler for RecorderRef {
        fn handle(&self, ctx: &HookContext) -> HookResult {
            self.0.handle(ctx)
        }
    }

    let mut registry = HookRegistry::new();
    registry.register(Box::new(RecorderRef(recorder.clone())));

    let _ = registry.dispatch(HookPayload::PreTurn {
        turn_id: "42".to_string(),
    });

    let captured = recorder.seen.lock().unwrap().clone().expect("handler ran");
    assert_eq!(captured.event, HookEvent::PreTurn);
    match captured.payload {
        HookPayload::PreTurn { turn_id } => assert_eq!(turn_id, "42"),
        other => panic!("unexpected payload {other:?}"),
    }
}

#[test]
fn enum_variants_are_distinct() {
    // Smoke test that every variant survives the round trip through
    // equality. Cheap insurance against accidentally collapsing a
    // variant during a future refactor.
    let events = [
        HookEvent::PreTurn,
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::PostToolUseFailure,
        HookEvent::PostTool,
        HookEvent::PreCompact,
        HookEvent::PostCompact,
        HookEvent::SubagentStart,
        HookEvent::SubagentStop,
        HookEvent::PermissionRequest,
        HookEvent::PermissionDenied,
        HookEvent::UserPromptSubmit,
        HookEvent::SessionStart,
        HookEvent::Stop,
        HookEvent::Setup,
    ];
    for (i, a) in events.iter().enumerate() {
        for (j, b) in events.iter().enumerate() {
            assert_eq!(a == b, i == j);
        }
    }
}

#[test]
fn payload_event_discriminant_matches_variant() {
    let cases: Vec<(HookPayload, HookEvent)> = vec![
        (
            HookPayload::PreTurn {
                turn_id: "1".into(),
            },
            HookEvent::PreTurn,
        ),
        (
            HookPayload::PreToolUse {
                turn_id: "1".into(),
                tool_name: "read_file".into(),
                call_id: "c".into(),
            },
            HookEvent::PreToolUse,
        ),
        (
            HookPayload::PostToolUse {
                turn_id: "1".into(),
                tool_name: "read_file".into(),
                call_id: "c".into(),
                status: "success".into(),
            },
            HookEvent::PostToolUse,
        ),
        (
            HookPayload::PostToolUseFailure {
                turn_id: "1".into(),
                tool_name: "read_file".into(),
                call_id: "c".into(),
                status: "error".into(),
                error: Some("boom".into()),
            },
            HookEvent::PostToolUseFailure,
        ),
        (
            HookPayload::PostTool {
                turn_id: "1".into(),
                tool_name: "read_file".into(),
                call_id: "c".into(),
                status: "success".into(),
            },
            HookEvent::PostTool,
        ),
        (
            HookPayload::PreCompact {
                turn_id: "1".into(),
                before_tokens: 100,
            },
            HookEvent::PreCompact,
        ),
        (
            HookPayload::PostCompact {
                turn_id: "1".into(),
                before_tokens: 100,
                after_tokens: 50,
            },
            HookEvent::PostCompact,
        ),
        (
            HookPayload::SubagentStart {
                subagent_id: "s1".into(),
                kind: "explore".into(),
                parent_turn_id: "1".into(),
            },
            HookEvent::SubagentStart,
        ),
        (
            HookPayload::SubagentStop {
                subagent_id: "s1".into(),
                kind: "explore".into(),
                parent_turn_id: "1".into(),
                status: "success".into(),
            },
            HookEvent::SubagentStop,
        ),
        (
            HookPayload::PermissionRequest {
                capability: "read".into(),
                tool_name: "read_file".into(),
                turn_id: "1".into(),
                call_id: "c".into(),
                target: None,
            },
            HookEvent::PermissionRequest,
        ),
        (
            HookPayload::PermissionDenied {
                capability: "read".into(),
                tool_name: "read_file".into(),
                turn_id: "1".into(),
                call_id: "c".into(),
                target: None,
                reason: "policy".into(),
            },
            HookEvent::PermissionDenied,
        ),
        (
            HookPayload::UserPromptSubmit {
                prompt: "hello".into(),
                turn_id: "1".into(),
            },
            HookEvent::UserPromptSubmit,
        ),
        (
            HookPayload::SessionStart {
                session_id: "s".into(),
                reason: "boot".into(),
            },
            HookEvent::SessionStart,
        ),
        (
            HookPayload::Stop {
                turn_id: "1".into(),
            },
            HookEvent::Stop,
        ),
        (
            HookPayload::Setup {
                workspace: "/tmp/ws".into(),
                reason: "boot".into(),
            },
            HookEvent::Setup,
        ),
    ];
    for (payload, expected) in cases {
        let ctx = HookContext::new(payload.clone());
        assert_eq!(ctx.event, expected, "ctx.event mismatch for {payload:?}");
        assert_eq!(payload.event(), expected, "payload.event() mismatch");
    }
}

#[test]
fn dispatch_pre_and_post_tool_use_round_trip_payloads() {
    /// Captures the JSON projection of each payload the handler sees
    /// so the test can verify the dispatcher forwarded variants in
    /// order without dropping fields.
    struct Recorder {
        seen: std::sync::Mutex<Vec<(HookEvent, Value)>>,
    }

    impl HookHandler for Recorder {
        fn handle(&self, ctx: &HookContext) -> HookResult {
            self.seen
                .lock()
                .unwrap()
                .push((ctx.event, ctx.payload_json()));
            HookResult::allow()
        }
    }

    let recorder = std::sync::Arc::new(Recorder {
        seen: std::sync::Mutex::new(Vec::new()),
    });
    struct RecorderRef(std::sync::Arc<Recorder>);
    impl HookHandler for RecorderRef {
        fn handle(&self, ctx: &HookContext) -> HookResult {
            self.0.handle(ctx)
        }
    }

    let mut registry = HookRegistry::new();
    registry.register(Box::new(RecorderRef(recorder.clone())));

    let _ = registry.dispatch(HookPayload::PreToolUse {
        turn_id: "7".to_string(),
        tool_name: "read_file".to_string(),
        call_id: "c1".to_string(),
    });
    let _ = registry.dispatch(HookPayload::PostToolUse {
        turn_id: "7".to_string(),
        tool_name: "read_file".to_string(),
        call_id: "c1".to_string(),
        status: "success".to_string(),
    });

    let seen = recorder.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].0, HookEvent::PreToolUse);
    assert_eq!(seen[0].1["tool_name"], "read_file");
    assert_eq!(seen[0].1["call_id"], "c1");
    assert_eq!(seen[0].1["turn_id"], "7");
    assert_eq!(seen[1].0, HookEvent::PostToolUse);
    assert_eq!(seen[1].1["status"], "success");
}

mod agent_hook_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;

    /// Hook that rewrites `payload["model"]` so tests can verify the
    /// bus threads a mutable `LlmRequestView` through every handler.
    struct ModelRewriter {
        new_model: &'static str,
    }

    impl AgentHook for ModelRewriter {
        fn before_provider_request<'a>(
            &'a self,
            req: &'a mut LlmRequestView,
        ) -> HookFuture<'a, ()> {
            Box::pin(async move {
                req.payload["model"] = Value::String(self.new_model.to_string());
            })
        }
    }

    /// Hook that records the model it observed at dispatch time so
    /// the test can verify ordering — earlier mutations must be
    /// visible to later handlers.
    struct ModelObserver {
        seen: Arc<Mutex<Option<String>>>,
    }

    impl AgentHook for ModelObserver {
        fn before_provider_request<'a>(
            &'a self,
            req: &'a mut LlmRequestView,
        ) -> HookFuture<'a, ()> {
            let seen = self.seen.clone();
            let snapshot = req
                .payload
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
            Box::pin(async move {
                *seen.lock().unwrap() = snapshot;
            })
        }
    }

    /// Hook that flips `arguments["path"]` and then allows the call;
    /// pairs with [`DenyingHook`] to verify ordering plus deny
    /// short-circuit.
    struct ArgumentPatcher;

    impl AgentHook for ArgumentPatcher {
        fn before_tool_call<'a>(&'a self, call: &'a mut ToolCallView) -> HookFuture<'a, Decision> {
            Box::pin(async move {
                call.arguments["path"] = Value::String("/patched".into());
                Decision::Allow
            })
        }
    }

    /// Hook that denies every tool call it sees.
    struct DenyingHook {
        reason: &'static str,
    }

    impl AgentHook for DenyingHook {
        fn before_tool_call<'a>(&'a self, _call: &'a mut ToolCallView) -> HookFuture<'a, Decision> {
            let reason = self.reason.to_string();
            Box::pin(async move { Decision::Deny { message: reason } })
        }
    }

    /// Hook that records `before_tool_call` invocations so tests can
    /// verify deny-short-circuit semantics.
    struct ToolCallCounter {
        calls: Arc<Mutex<u32>>,
    }

    impl AgentHook for ToolCallCounter {
        fn before_tool_call<'a>(&'a self, _call: &'a mut ToolCallView) -> HookFuture<'a, Decision> {
            let calls = self.calls.clone();
            Box::pin(async move {
                *calls.lock().unwrap() += 1;
                Decision::Allow
            })
        }
    }

    /// Hook that wraps `output` in `{"redacted": <original>}` to
    /// verify the result mutation contract.
    struct ResultRedactor;

    impl AgentHook for ResultRedactor {
        fn after_tool_result<'a>(&'a self, result: &'a mut ToolResultView) -> HookFuture<'a, ()> {
            Box::pin(async move {
                let prior = std::mem::replace(&mut result.output, Value::Null);
                result.output = json!({ "redacted": prior });
            })
        }
    }

    #[tokio::test]
    async fn bus_threads_mutable_request_through_handlers_in_order() {
        let observed: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let mut bus = AgentHookBus::new();
        bus.register(Box::new(ModelRewriter {
            new_model: "gpt-mini",
        }));
        bus.register(Box::new(ModelObserver {
            seen: observed.clone(),
        }));

        let mut req = LlmRequestView::new("turn-1", json!({ "model": "gpt-default" }));
        bus.before_provider_request(&mut req).await;

        assert_eq!(req.payload["model"], Value::String("gpt-mini".into()));
        assert_eq!(observed.lock().unwrap().as_deref(), Some("gpt-mini"));
    }

    #[tokio::test]
    async fn empty_bus_passes_request_through_unchanged() {
        let bus = AgentHookBus::new();
        let original = json!({ "model": "gpt-default" });
        let mut req = LlmRequestView::new("turn-1", original.clone());
        bus.before_provider_request(&mut req).await;
        assert!(bus.is_empty());
        assert_eq!(req.payload, original);
    }

    #[tokio::test]
    async fn before_tool_call_short_circuits_on_first_deny() {
        let counter = Arc::new(Mutex::new(0u32));
        let mut bus = AgentHookBus::new();
        bus.register(Box::new(ArgumentPatcher));
        bus.register(Box::new(DenyingHook {
            reason: "policy violation",
        }));
        bus.register(Box::new(ToolCallCounter {
            calls: counter.clone(),
        }));

        let mut call = ToolCallView::new("turn-1", "call-1", "edit", json!({ "path": "/orig" }));
        let decision = bus.before_tool_call(&mut call).await;

        match decision {
            Decision::Deny { message } => assert_eq!(message, "policy violation"),
            Decision::Allow => panic!("expected deny"),
        }
        // The patcher ran before the deny, so the mutation is visible
        // even though the call won't proceed.
        assert_eq!(call.arguments["path"], Value::String("/patched".into()));
        // The counter sits *after* the deny in the registration
        // order, so the bus must have short-circuited and skipped it.
        assert_eq!(*counter.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn after_tool_result_can_mutate_output() {
        let mut bus = AgentHookBus::new();
        bus.register(Box::new(ResultRedactor));

        let mut result = ToolResultView::new(
            "turn-1",
            "call-1",
            "read",
            "success",
            json!({ "bytes": 42 }),
        );
        bus.after_tool_result(&mut result).await;

        assert_eq!(result.output, json!({ "redacted": { "bytes": 42 } }));
    }

    /// Legacy `HookHandler` that records every payload it sees so we
    /// can verify the typed forwarder bridges into the observation
    /// surface faithfully.
    struct RecorderHandler {
        seen: Arc<Mutex<Vec<(HookEvent, Value)>>>,
    }

    impl HookHandler for RecorderHandler {
        fn handle(&self, ctx: &HookContext) -> HookResult {
            self.seen
                .lock()
                .unwrap()
                .push((ctx.event, ctx.payload_json()));
            HookResult::allow()
        }
    }

    /// Legacy `HookHandler` that denies `PreToolUse` so the forwarder
    /// can be exercised end-to-end through the typed dispatch path.
    struct DenyingLegacyHandler;

    impl HookHandler for DenyingLegacyHandler {
        fn handle(&self, ctx: &HookContext) -> HookResult {
            if ctx.event == HookEvent::PreToolUse {
                HookResult::deny("legacy block")
            } else {
                HookResult::allow()
            }
        }
    }

    #[tokio::test]
    async fn legacy_forwarder_bridges_typed_events_into_observation_registry() {
        let seen: Arc<Mutex<Vec<(HookEvent, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let mut registry = HookRegistry::new();
        registry.register(Box::new(RecorderHandler { seen: seen.clone() }));

        let forwarder = LegacyHookForwarder::new(Arc::new(registry));
        let mut bus = AgentHookBus::new();
        bus.register(Box::new(forwarder));

        let mut req = LlmRequestView::new("turn-7", json!({ "model": "x" }));
        bus.before_provider_request(&mut req).await;

        let mut call = ToolCallView::new("turn-7", "call-1", "read", json!({}));
        let decision = bus.before_tool_call(&mut call).await;
        assert!(decision.is_allow());

        let mut result = ToolResultView::new("turn-7", "call-1", "read", "success", json!({}));
        bus.after_tool_result(&mut result).await;

        let observed = seen.lock().unwrap().clone();
        assert_eq!(observed.len(), 3);
        assert_eq!(observed[0].0, HookEvent::PreTurn);
        assert_eq!(observed[0].1["turn_id"], "turn-7");
        assert_eq!(observed[1].0, HookEvent::PreToolUse);
        assert_eq!(observed[1].1["turn_id"], "turn-7");
        assert_eq!(observed[1].1["tool_name"], "read");
        assert_eq!(observed[1].1["call_id"], "call-1");
        assert_eq!(observed[2].0, HookEvent::PostToolUse);
        assert_eq!(observed[2].1["turn_id"], "turn-7");
        assert_eq!(observed[2].1["tool_name"], "read");
        assert_eq!(observed[2].1["call_id"], "call-1");
        assert_eq!(observed[2].1["status"], "success");
    }

    #[tokio::test]
    async fn legacy_forwarder_propagates_deny_as_typed_decision() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(DenyingLegacyHandler));
        let forwarder = LegacyHookForwarder::new(Arc::new(registry));

        let mut call = ToolCallView::new("turn-1", "call-1", "write", json!({}));
        let decision = forwarder.before_tool_call(&mut call).await;

        match decision {
            Decision::Deny { message } => assert_eq!(message, "legacy block"),
            Decision::Allow => panic!("expected deny"),
        }
    }

    #[tokio::test]
    async fn legacy_forwarder_skips_dispatch_for_empty_registry() {
        let forwarder = LegacyHookForwarder::new(Arc::new(HookRegistry::new()));
        let mut req = LlmRequestView::new("turn-1", json!({}));
        forwarder.before_provider_request(&mut req).await;

        let mut call = ToolCallView::new("turn-1", "call-1", "noop", json!({}));
        assert!(forwarder.before_tool_call(&mut call).await.is_allow());

        let mut result = ToolResultView::new("turn-1", "call-1", "noop", "success", json!({}));
        forwarder.after_tool_result(&mut result).await;
    }

    #[tokio::test]
    async fn default_agent_hook_methods_are_no_ops() {
        struct DefaultHook;
        impl AgentHook for DefaultHook {}

        let mut bus = AgentHookBus::new();
        bus.register(Box::new(DefaultHook));
        assert_eq!(bus.len(), 1);

        let original_req = json!({ "model": "x" });
        let mut req = LlmRequestView::new("turn-1", original_req.clone());
        bus.before_provider_request(&mut req).await;
        assert_eq!(req.payload, original_req);

        let original_args = json!({ "k": "v" });
        let mut call = ToolCallView::new("turn-1", "call-1", "tool", original_args.clone());
        let decision = bus.before_tool_call(&mut call).await;
        assert!(decision.is_allow());
        assert_eq!(call.arguments, original_args);

        let original_output = json!({ "ok": true });
        let mut result = ToolResultView::new(
            "turn-1",
            "call-1",
            "tool",
            "success",
            original_output.clone(),
        );
        bus.after_tool_result(&mut result).await;
        assert_eq!(result.output, original_output);
    }
}
