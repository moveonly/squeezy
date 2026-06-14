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
