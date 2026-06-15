//! Terminal persistence helpers for `TurnRuntime`.
//!
//! This phase records turn telemetry, drains terminal side channels, merges
//! session metrics, and dispatches stop hooks after durable turn accounting.

use super::super::*;

impl TurnRuntime {
    pub(crate) async fn finish_turn(&self, metrics: &TurnMetrics) {
        // Record turn_completed while the span is still open so the event
        // carries the per-turn span_id. Use `record()` (awaited) rather than
        // `spawn()` so the event is persisted to the durable ledger before
        // finish_session / flush_telemetry read from it.
        self.telemetry
            .record(TelemetryEvent::turn_completed(
                &self.config,
                self.turn_id.get(),
                metrics.clone(),
            ))
            .await;
        self.telemetry.end_turn();
        // Drain MCP elicitation audit ring and emit per-elicitation events.
        emit_mcp_elicitation_telemetry(&self.tools, &self.telemetry);
        self.session_metrics.lock().await.merge_turn(metrics);
        // Stop fires after telemetry persistence so audit handlers
        // see the final TurnMetrics already on disk.
        self.dispatch_stop();
    }
}
