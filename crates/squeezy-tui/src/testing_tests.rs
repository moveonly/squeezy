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

/// Minimal provider stub: returns its configured name and never
/// streams. Lets the harness build without a live LLM, so the test
/// can render a frame and read the banner-derived model row.
struct NamedProvider(&'static str);

impl LlmProvider for NamedProvider {
    fn name(&self) -> &'static str {
        self.0
    }
    fn stream_response(&self, _request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        use futures_util::stream;
        Box::pin(stream::iter(Vec::new()))
    }
}

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
    TuiHarness::new(config, SessionMode::Build, provider, 80, 24, None)
        .expect("harness builds with stub provider")
}

#[test]
fn harness_status_line_uses_real_provider_name_not_eval_harness() {
    // `model` / `languages` no longer appear in the startup banner —
    // both are now status-line items so they stay live across config
    // edits (inline scrollback can't be rewritten). The opt-in
    // `provider-and-model` item carries the provider:model pair.
    let mut config = AppConfig {
        model: "test-model".to_string(),
        ..AppConfig::default()
    };
    config.tui.status_line = Some(vec!["provider-and-model".to_string()]);
    let provider: Arc<dyn LlmProvider> = Arc::new(NamedProvider("anthropic"));
    let mut harness = TuiHarness::new(config, SessionMode::default(), provider, 120, 36, None)
        .expect("build TuiHarness");
    let snapshot = harness.render_frame().expect("render frame");
    let plain = snapshot.plain_text;
    assert!(
        plain.contains("anthropic:test-model"),
        "expected status line to contain `anthropic:test-model`, frame was:\n{plain}"
    );
    assert!(
        !plain.contains("eval-harness:"),
        "rendering still carries the harness literal `eval-harness:`; frame was:\n{plain}"
    );
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
    harness.app_mut().pending_approval = Some(PendingApproval {
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

/// Real-ANSI capture harness (§8 term-matrix). Proves `drive_scenario`
/// builds a `CaptureLog` over the `Capture` sink, records one
/// `FrameMark` per `Step::Frame` (in paint order, with the in-effect
/// size), and that the captured byte stream is the real append-only
/// ANSI — wrapped in the DEC 2026 synchronized-update markers, and
/// self-slicing by the recorded offsets.
#[cfg(feature = "term-matrix")]
#[tokio::test]
async fn drive_scenario_records_capture_log_frame_marks() {
    use crate::termsim::Step;

    let mut harness = build_harness();
    let log = harness
        .drive_scenario(&[
            // First frame at the build size (80x24).
            Step::Frame,
            // Land a committed assistant turn, then shrink and paint.
            Step::AssistantDelta("hello from the model".to_string()),
            Step::Resize(100, 30),
            Step::Frame,
            // A tool-output line lands as history, paint a third frame.
            Step::ToolOutput("ran: grep -n foo".to_string()),
            Step::Frame,
        ])
        .await
        .expect("drive_scenario produces a CaptureLog");

    // One mark per `Step::Frame`, in paint order.
    assert_eq!(log.frames.len(), 3, "one FrameMark per Frame step");

    // The first frame painted at the build size; the next two after the
    // resize to 100x30.
    assert_eq!((log.frames[0].w, log.frames[0].h), (80, 24));
    assert_eq!((log.frames[1].w, log.frames[1].h), (100, 30));
    assert_eq!((log.frames[2].w, log.frames[2].h), (100, 30));

    // Offsets are recorded AFTER each paint flushes, so they are
    // strictly increasing (every frame emits at least the sync-update
    // markers + a footer) and bounded by the full stream length.
    assert!(
        log.frames[0].byte_offset < log.frames[1].byte_offset,
        "frame offsets must advance: {:?}",
        log.frames,
    );
    assert!(
        log.frames[1].byte_offset < log.frames[2].byte_offset,
        "frame offsets must advance: {:?}",
        log.frames,
    );
    assert!(
        log.frames[2].byte_offset <= log.bytes.len(),
        "last frame offset is within the captured stream",
    );
    assert!(!log.bytes.is_empty(), "the capture sink teed real bytes");

    // The log is self-slicing per frame: frame 0 is bytes[0..mark0],
    // and the second frame's slice carries real append-only output
    // including the DEC 2026 synchronized-update BEGIN marker (`\x1b[?2026h`).
    let frame1 = &log.bytes[log.frames[0].byte_offset..log.frames[1].byte_offset];
    let begin_sync = b"\x1b[?2026h";
    assert!(
        frame1.windows(begin_sync.len()).any(|w| w == begin_sync),
        "each painted frame opens a synchronized update",
    );
}
