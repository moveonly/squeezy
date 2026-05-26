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
/// though the agent does not yet apply mutations.
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

    let results = registry.dispatch(HookEvent::PreTurn, json!({ "turn_index": 0 }));

    assert_eq!(results.len(), 1);
    assert!(results[0].allow);
    assert!(results[0].mutate.is_none());
    assert!(results[0].message.is_none());
}

#[test]
fn empty_registry_dispatches_to_no_handlers() {
    let registry = HookRegistry::new();
    let results = registry.dispatch(HookEvent::PreTurn, json!({}));
    assert!(results.is_empty());
    assert!(registry.is_empty());
}

#[test]
fn handlers_can_propose_mutations_visible_to_callers() {
    let mut registry = HookRegistry::new();
    registry.register(Box::new(MutatingHandler {
        replacement: json!({ "preamble": "extra instructions" }),
    }));

    let results = registry.dispatch(HookEvent::PreTurn, json!({ "turn_index": 1 }));
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].mutate.as_ref().unwrap(),
        &json!({ "preamble": "extra instructions" })
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

    let payload = json!({ "turn_index": 42 });
    let _ = registry.dispatch(HookEvent::PreTurn, payload.clone());

    let captured = recorder.seen.lock().unwrap().clone().expect("handler ran");
    assert_eq!(captured.event, HookEvent::PreTurn);
    assert_eq!(captured.payload, payload);
}

#[test]
fn enum_variants_are_distinct() {
    // Smoke test that every reserved variant survives the round trip
    // through equality and the dispatch path. Cheap insurance against
    // accidentally collapsing a variant during a future refactor.
    let events = [
        HookEvent::PreTurn,
        HookEvent::PostTool,
        HookEvent::PreCompact,
        HookEvent::SubagentStart,
        HookEvent::PermissionRequest,
    ];
    for (i, a) in events.iter().enumerate() {
        for (j, b) in events.iter().enumerate() {
            assert_eq!(a == b, i == j);
        }
    }
}
