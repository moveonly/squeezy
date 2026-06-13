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

#[test]
fn compact_startup_card_keeps_provider_model_badge() {
    // Compact density suppresses the status detail line, the only place
    // provider:model is shown. The empty startup frame must still surface it,
    // so the card records a dim badge.
    let config = AppConfig {
        model: "test-model".to_string(),
        ..AppConfig::default()
    };
    let provider: Arc<dyn LlmProvider> = Arc::new(NamedProvider("anthropic"));
    let mut harness = TuiHarness::new(config, SessionMode::default(), provider, 80, 32, None)
        .expect("build TuiHarness");
    harness.app_mut().density_override = crate::density::DensityMode::Compact;
    let snapshot = harness.render_frame().expect("render frame");
    let plain = snapshot.plain_text;
    assert!(
        plain.contains("anthropic:test-model"),
        "compact startup frame must still carry `anthropic:test-model`, frame was:\n{plain}"
    );
}

#[test]
fn default_startup_card_does_not_duplicate_model_badge() {
    // At default density the detail line carries provider:model, so the startup
    // card must not add a second copy.
    let config = AppConfig {
        model: "test-model".to_string(),
        ..AppConfig::default()
    };
    let provider: Arc<dyn LlmProvider> = Arc::new(NamedProvider("anthropic"));
    let mut harness = TuiHarness::new(config, SessionMode::default(), provider, 120, 36, None)
        .expect("build TuiHarness");
    harness.app_mut().density_override = crate::density::DensityMode::Default;
    let snapshot = harness.render_frame().expect("render frame");
    let plain = snapshot.plain_text;
    assert_eq!(
        plain.matches("anthropic:test-model").count(),
        1,
        "default density must show provider:model exactly once (detail line only), frame was:\n{plain}"
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

/// Regression for deep-review #117. `pump_until_idle` used to drain the queue
/// with a bare `pop_front` + `start_user_turn`, bypassing the production
/// `drain_prompt_queue_if_idle` gating. A `Manual`-conditioned front prompt must
/// be LEFT parked (never auto-run) — the bare pop would have run it.
#[tokio::test]
async fn pump_until_idle_leaves_a_manual_conditioned_prompt_parked() {
    let mut harness = build_harness();
    {
        let app = harness.app_mut();
        app.prompt_queue.push_back("manual-only".to_string());
        crate::enqueue_queue_id(app);
        let id = *app.prompt_queue_ids.front().expect("id stamped");
        app.prompt_queue_conditions
            .set(id, crate::queue_conditions::QueueCondition::Manual);
        // Arm the pump exactly as a turn-finish would.
        app.auto_drain_queue = true;
    }

    tokio::time::timeout(std::time::Duration::from_secs(2), harness.pump_until_idle())
        .await
        .expect("pump must return promptly, not spin on the parked prompt")
        .expect("pump_until_idle returns Ok with a parked prompt");

    // The Manual prompt is still queued (parked), and no model turn was started.
    assert!(
        !harness.is_turn_active(),
        "a Manual-conditioned prompt must not be auto-run by the harness pump",
    );
    let app = harness.app_mut();
    assert_eq!(
        app.prompt_queue.iter().cloned().collect::<Vec<_>>(),
        vec!["manual-only".to_string()],
        "the parked prompt stays in the queue",
    );
}

/// Companion to the above: a queued slash command must be DISPATCHED as a command
/// (via `submit_queued_input`), not started as a raw model turn. The bare-pop
/// harness path sent the slash text straight to `start_user_turn`.
#[tokio::test]
async fn pump_until_idle_dispatches_a_queued_slash_command() {
    let mut harness = build_harness();
    {
        let app = harness.app_mut();
        app.push_transcript_item(crate::TranscriptItem::user("old context"));
        app.prompt_queue.push_back("/clear".to_string());
        crate::enqueue_queue_id(app);
        app.auto_drain_queue = true;
    }

    tokio::time::timeout(std::time::Duration::from_secs(2), harness.pump_until_idle())
        .await
        .expect("pump must return promptly")
        .expect("pump_until_idle returns Ok after dispatching the slash command");

    assert!(
        !harness.is_turn_active(),
        "a queued /clear is a command, not a model turn",
    );
    let app = harness.app_mut();
    assert!(
        app.prompt_queue.is_empty(),
        "the queued slash entry was consumed by the drain",
    );
    assert!(
        app.terminal_clear_pending,
        "/clear was dispatched as a command (it requested a hard terminal clear)",
    );
}

/// Regression for deep-review #78: `TuiHarness::new` must mirror `run_inner`'s
/// pre-paint preamble and restore the saved per-workspace UI profile (§12.7.4)
/// — but only when the profile store is explicitly pinned via
/// `SQUEEZY_UI_PROFILE_DIR`, so eval stays deterministic and never reads the
/// operator's real `~/.squeezy/projects`. Before the fix the harness stopped at
/// `apply_theme_overrides` -> `Agent::new` -> `new_with_clipboard` and never
/// restored, so `show_minimap` stayed at its `false` default even with a saved
/// profile flipping it on.
#[test]
fn harness_restores_workspace_profile_when_store_pinned() {
    use crate::workspace_profile;

    // Serialize against the shared profile-dir lock (the same one the unit and
    // integration profile tests take) so no other test mutates the global
    // `SQUEEZY_UI_PROFILE_DIR` while we have it pinned.
    let _lock = workspace_profile::PROFILE_DIR_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let prior = std::env::var_os(workspace_profile::PROFILE_DIR_ENV);

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let store = std::env::temp_dir().join(format!("squeezy_harness_profile_store_{nonce}"));
    let workspace = std::env::temp_dir().join(format!("squeezy_harness_profile_ws_{nonce}"));
    std::fs::create_dir_all(&workspace).expect("create workspace");

    // SAFETY: serialized by `PROFILE_DIR_TEST_LOCK` held in `_lock`.
    unsafe {
        std::env::set_var(workspace_profile::PROFILE_DIR_ENV, &store);
    }

    // Save a profile for this workspace that flips `minimap` ON (its app default
    // is `false`), so a successful restore is observable as `show_minimap == true`.
    let profile = workspace_profile::UiProfile {
        version: workspace_profile::PROFILE_SCHEMA_VERSION,
        minimap: Some(true),
        ..Default::default()
    };
    workspace_profile::save(&workspace, &profile).expect("save profile to scratch store");

    let config = AppConfig {
        model: "stub-model".to_string(),
        workspace_root: workspace.clone(),
        ..AppConfig::default()
    };
    let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider);
    let mut harness = TuiHarness::new(config, SessionMode::Build, provider, 80, 24, None)
        .expect("harness builds with stub provider");

    assert!(
        harness.app_mut().show_minimap,
        "harness must restore the saved per-workspace profile (minimap flipped on)",
    );

    // SAFETY: serialized by `PROFILE_DIR_TEST_LOCK` held in `_lock`; restore prior.
    unsafe {
        match &prior {
            Some(prev) => std::env::set_var(workspace_profile::PROFILE_DIR_ENV, prev),
            None => std::env::remove_var(workspace_profile::PROFILE_DIR_ENV),
        }
    }
    let _ = std::fs::remove_dir_all(&store);
    let _ = std::fs::remove_dir_all(&workspace);
}

/// Companion to the above: with the profile store NOT pinned (the production-eval
/// default), the harness must SKIP the restore so eval runs never read the real
/// `~/.squeezy/projects` tree and stay deterministic. `show_minimap` keeps its
/// `false` default regardless of what any on-disk profile says.
#[test]
fn harness_skips_workspace_profile_when_store_unpinned() {
    use crate::workspace_profile;

    let _lock = workspace_profile::PROFILE_DIR_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let prior = std::env::var_os(workspace_profile::PROFILE_DIR_ENV);
    // SAFETY: serialized by `PROFILE_DIR_TEST_LOCK` held in `_lock`.
    unsafe {
        std::env::remove_var(workspace_profile::PROFILE_DIR_ENV);
    }

    let mut harness = build_harness();
    assert!(
        !harness.app_mut().show_minimap,
        "with the store unpinned the harness must not restore any profile",
    );

    // SAFETY: serialized by `PROFILE_DIR_TEST_LOCK` held in `_lock`; restore prior.
    unsafe {
        if let Some(prev) = &prior {
            std::env::set_var(workspace_profile::PROFILE_DIR_ENV, prev);
        }
    }
}
