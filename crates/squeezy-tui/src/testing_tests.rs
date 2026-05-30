use super::*;
use crate::PendingApproval;
use squeezy_agent::ToolApprovalRequest;
use squeezy_core::{
    AppConfig, PermissionCapability, PermissionRequest, PermissionRisk, PermissionScope,
    SessionMode,
};
use squeezy_llm::{LlmProvider, LlmRequest, LlmStream};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

struct StubProvider;

impl LlmProvider for StubProvider {
    fn name(&self) -> &'static str {
        "stub"
    }

    fn stream_response(&self, _request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        // The test never triggers a turn — pump_until_idle is exercised
        // against a hand-injected pending_approval, not a live model
        // stream. An empty stream is a stand-in that never produces
        // events and never panics.
        Box::pin(futures_util::stream::empty())
    }
}

fn sample_request() -> ToolApprovalRequest {
    ToolApprovalRequest {
        id: 1,
        call_id: "call-1".to_string(),
        tool_name: "grep".to_string(),
        scope: PermissionScope::IgnoredSearch,
        permission: PermissionRequest {
            call_id: "call-1".to_string(),
            tool_name: "grep".to_string(),
            capability: PermissionCapability::Search,
            target: "Agent".to_string(),
            risk: PermissionRisk::Low,
            summary: "grep approval".to_string(),
            metadata: BTreeMap::new(),
            suggested_rules: Vec::new(),
        },
        matched_rule: None,
        reason: "test".to_string(),
        context: None,
        preview: Vec::new(),
    }
}

fn temp_workspace() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy_tui_harness_test_{nonce}"));
    std::fs::create_dir_all(&root).expect("create temp workspace");
    root
}

fn build_harness() -> TuiHarness {
    // Keep the harness fixture off the real workspace and away from the
    // user's session log so `Agent::new` doesn't crawl the repo or
    // scribble into `~/.squeezy/sessions/...` on every test run.
    let config = AppConfig {
        model: "stub-model".to_string(),
        workspace_root: temp_workspace(),
        ..AppConfig::default()
    };
    let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider);
    TuiHarness::new(config, SessionMode::Build, provider, 80, 24)
        .expect("harness builds with stub provider")
}

/// Regression for squeezy-tje9. Before the fix `pump_until_idle` burned
/// its 180s deadline on a parked `pending_approval` because the loop
/// only watched `turn_rx`/`prompt_queue`. The harness must:
///   1. exit cleanly while the modal is open (so the eval driver can
///      route its queued `Approve` action),
///   2. clear the slot on `respond_approval`, and
///   3. exit cleanly again on the second pump (no turn, no modal).
#[tokio::test]
async fn pump_until_idle_yields_on_pending_approval_then_respond_clears() {
    let mut harness = build_harness();

    // Hand-inject the modal the production drain installs on
    // AgentEvent::ApprovalRequested. We hold the receiver so the test
    // can observe what decision flows out.
    let (decision_tx, decision_rx) = oneshot::channel();
    harness.app.pending_approval = Some(PendingApproval {
        request: sample_request(),
        decision_tx,
    });

    let start = std::time::Instant::now();
    let outcome =
        tokio::time::timeout(std::time::Duration::from_secs(2), harness.pump_until_idle())
            .await
            .expect("pump must return early, not park on the 180s deadline");
    outcome.expect("pump_until_idle returns Ok while a modal is open");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(1),
        "pump_until_idle should yield ~immediately, not sleep until the deadline",
    );
    assert!(
        harness.has_pending_approval(),
        "modal slot should still be live so the driver can answer it",
    );
    assert_eq!(harness.pending_approval_tool(), Some("grep"));

    assert!(
        harness.respond_approval(),
        "respond_approval returns true when a slot was consumed",
    );
    assert!(
        !harness.has_pending_approval(),
        "respond_approval clears the pending_approval slot",
    );
    let decision = decision_rx
        .await
        .expect("decision_tx must deliver before being dropped");
    assert_eq!(decision, squeezy_agent::ToolApprovalDecision::Approved);

    // After the approval is answered the harness drains the (empty)
    // approved-tool follow-up and lands back in idle.
    tokio::time::timeout(std::time::Duration::from_secs(2), harness.pump_until_idle())
        .await
        .expect("second pump completes within the timeout")
        .expect("second pump_until_idle returns Ok");
    assert!(!harness.has_pending_approval());
    assert!(!harness.is_turn_active());

    // respond_approval on an empty slot returns false rather than
    // pretending it routed a decision.
    assert!(!harness.respond_approval());
    assert!(!harness.respond_deny());
}
