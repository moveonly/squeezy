//! Scenario driver: replays a [`Scenario`] against the real append-only paint
//! path and captures the emitted ANSI into a [`CaptureLog`].
//!
//! This is the only `termsim` file that touches `TuiHarness`. It builds a
//! headless harness sized to the scenario's `initial_size`, drives the scripted
//! steps through [`TuiHarness::drive_scenario`] (which feeds a `FixedSize` per
//! `Resize` step and tees the emitted bytes out of the shared `Capture` sink),
//! and returns the resulting [`CaptureLog`]. Because `term-matrix` implies
//! `testing`, the harness surface is in scope here.
//!
//! No real PTY, no network, no live model: the harness is built around a stub
//! `LlmProvider` and a per-run scratch workspace, so a run is fully
//! deterministic and reproducible.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use squeezy_core::{AppConfig, SessionMode};
use squeezy_llm::{LlmProvider, LlmRequest, LlmStream};
use tokio_util::sync::CancellationToken;

use super::scenario::Scenario;
use super::types::CaptureLog;
use crate::testing::{FrameSnapshot, TuiHarness};

/// Minimal `LlmProvider` for the matrix: it never streams. Every scenario
/// drives the transcript directly via `AssistantDelta` / `ToolOutput` steps
/// (which inject committed items), so the provider only has to exist and name
/// itself — it never needs to produce model events.
struct StubProvider;

impl LlmProvider for StubProvider {
    fn name(&self) -> &'static str {
        "termsim-stub"
    }

    fn stream_response(&self, _request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        Box::pin(futures_util::stream::empty())
    }
}

/// A throwaway workspace under the system temp dir, unique per call, so a
/// matrix run never crawls the real repo or writes into the operator's
/// `~/.squeezy/sessions`.
fn scratch_workspace() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("squeezy_termsim_{nonce}"));
    let _ = std::fs::create_dir_all(&root);
    root
}

/// The product of driving one scenario: the captured inline ANSI stream (for
/// the emulator backends) plus the settled fullscreen `render()` snapshot at
/// the final size.
///
/// The two surfaces answer different invariant questions. The append-only
/// inline stream (`log`) is what real terminals replay, so the emulator legs
/// assert cursor-in-bounds and no horizon stacking against *its* reconstructed
/// grid — and, on reflow-capable legs (alacritty) that rebuild the committed
/// scrollback, latest-response survival too. The fullscreen `render()` snapshot
/// (`final_frame`) is the main-view surface still in use at this stage of the
/// migration (§8.2 note): the composer-horizon / turn-divider invariants assert
/// against it, as does latest-response for the fixed-grid vt100 leg, which keeps
/// no scrollback the emulator grid could surface.
pub(crate) struct ScenarioRun {
    /// The captured append-only ANSI byte stream + per-frame marks.
    pub log: CaptureLog,
    /// The settled fullscreen `render()` frame at the scenario's final size.
    pub final_frame: FrameSnapshot,
    /// The known tail of the scenario's latest committed assistant response,
    /// or empty when the scenario commits none. Used by the latest-response
    /// invariant as a concrete needle.
    pub latest_response_tail: String,
}

/// Drive `scenario`'s steps against the real append-only paint path, returning
/// both the captured ANSI stream (plus per-`Frame` markers) and the settled
/// fullscreen render.
///
/// Builds a deterministic headless [`TuiHarness`] at the scenario's
/// `initial_size`, then runs the scripted steps on a fresh single-threaded
/// tokio runtime (the harness's pump/key paths are async). The matrix runner is
/// otherwise synchronous, so this owns its runtime rather than requiring callers
/// to be `async`.
pub(crate) fn run_scenario(scenario: &Scenario) -> ScenarioRun {
    let (w, h) = scenario.initial_size;
    let config = AppConfig {
        model: "termsim-model".to_string(),
        workspace_root: scratch_workspace(),
        ..AppConfig::default()
    };
    let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider);
    let mut harness = TuiHarness::new(config, SessionMode::Build, provider, w, h, None)
        .expect("termsim harness builds with stub provider");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("termsim tokio runtime");

    let log = runtime
        .block_on(harness.drive_scenario(&scenario.steps))
        .expect("drive_scenario produces a CaptureLog");

    // The settled fullscreen view at the final size: `render_frame` re-renders
    // through the same `render()` the production main view drives, so the
    // composer-horizon / latest-response invariants assert against exactly what
    // a user sees on the active surface today.
    let final_frame = harness
        .render_frame()
        .expect("render the settled fullscreen frame");

    ScenarioRun {
        log,
        final_frame,
        latest_response_tail: scenario.latest_response_tail().unwrap_or_default(),
    }
}
