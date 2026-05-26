//! Status-bar segment composition.
//!
//! Replaces the previous monolithic `format_status_details` with a list of
//! per-concern segment functions. Each segment returns `Option<String>`
//! (None = hide this frame); the composer joins the present segments with
//! the existing `"  "` separator. Adding a new status field is a one-file
//! change here.

use crate::{TuiApp, context_window_pct};

/// Render the full detail line. Preserves the historical format so existing
/// tests and consumers keep working.
pub(crate) fn render_status_details(app: &TuiApp) -> String {
    let segments: [Option<String>; 18] = [
        segments::permissions(app),
        segments::repo(app),
        segments::sandbox(app),
        segments::telemetry(app),
        segments::mcp(app),
        segments::cost(app),
        segments::tokens(app),
        segments::context(app),
        segments::pins(app),
        segments::compact(app),
        segments::tools(app),
        segments::budget(app),
        segments::config(app),
        segments::bytes_read(app),
        segments::receipts(app),
        segments::redactions(app),
        segments::cached_tokens(app),
        segments::cache_write_tokens(app),
    ];
    let pieces: Vec<String> = segments.into_iter().flatten().collect();
    pieces.join("  ")
}

/// Render the `cost ...` segment with optional cap and percent. Kept as a
/// free function so unit tests can exercise the format string without
/// having to construct a full `TuiApp`. When `cap_usd_micros` is `None` or
/// zero, falls back to the historical `cost $X.XXXXXX` form.
pub(crate) fn format_cost_segment(
    cost: &squeezy_core::CostSnapshot,
    cap_usd_micros: Option<u64>,
) -> String {
    use crate::commands::format_cost;
    match cap_usd_micros {
        Some(cap) if cap > 0 => {
            let spent = cost.estimated_usd_micros.unwrap_or(0);
            let percent = if cap == 0 {
                0
            } else {
                ((spent as u128 * 100) / cap as u128).min(255) as u8
            };
            format!(
                "cost {} / ${:.2} ({}%)",
                format_cost(cost),
                cap as f64 / 1_000_000.0,
                percent
            )
        }
        _ => format!("cost {}", format_cost(cost)),
    }
}

pub(crate) mod segments {
    use super::*;
    use crate::commands::format_optional_u64;
    use crate::{format_mcp_status, reasoning_status_fragment};

    pub(crate) fn permissions(app: &TuiApp) -> Option<String> {
        Some(app.permissions.compact())
    }

    pub(crate) fn repo(app: &TuiApp) -> Option<String> {
        Some(format!("repo {}", app.repo.detail()))
    }

    pub(crate) fn sandbox(app: &TuiApp) -> Option<String> {
        Some(format!("sandbox {}", app.permissions.sandbox))
    }

    pub(crate) fn telemetry(app: &TuiApp) -> Option<String> {
        Some(format!("telemetry {}", app.telemetry.as_str()))
    }

    pub(crate) fn mcp(app: &TuiApp) -> Option<String> {
        Some(format!("mcp {}", format_mcp_status(app)))
    }

    pub(crate) fn cost(app: &TuiApp) -> Option<String> {
        Some(format_cost_segment(&app.cost, app.cost_cap_usd_micros))
    }

    pub(crate) fn tokens(app: &TuiApp) -> Option<String> {
        Some(format!(
            "tok {}/{}{}",
            format_optional_u64(app.cost.input_tokens),
            format_optional_u64(app.cost.output_tokens),
            reasoning_status_fragment(app),
        ))
    }

    pub(crate) fn context(app: &TuiApp) -> Option<String> {
        let used = app.context_estimate.estimated_tokens;
        if app.context_compaction_threshold == 0 {
            return Some(format!("ctx {used}"));
        }
        let pct = context_window_pct(used, app.context_compaction_threshold);
        Some(format!(
            "ctx {used}/{threshold} ({pct}%)",
            threshold = app.context_compaction_threshold,
        ))
    }

    pub(crate) fn pins(app: &TuiApp) -> Option<String> {
        Some(format!("pins {}", app.context_compaction.pinned.len()))
    }

    pub(crate) fn compact(app: &TuiApp) -> Option<String> {
        Some(format!("compact {}", app.context_compaction.generation))
    }

    pub(crate) fn tools(app: &TuiApp) -> Option<String> {
        Some(format!("tools {}", app.metrics.tool_calls))
    }

    pub(crate) fn budget(app: &TuiApp) -> Option<String> {
        let label = if app.metrics.budget_denials == 0 {
            "ok".to_string()
        } else {
            format!("denied:{}", app.metrics.budget_denials)
        };
        Some(format!("budget {label}"))
    }

    pub(crate) fn config(app: &TuiApp) -> Option<String> {
        Some(format!("cfg {}", app.config_sources))
    }

    pub(crate) fn bytes_read(app: &TuiApp) -> Option<String> {
        Some(format!("read {}B", app.metrics.bytes_read))
    }

    pub(crate) fn receipts(app: &TuiApp) -> Option<String> {
        let total = app.metrics.receipt_stub_hits + app.metrics.negative_receipt_hits;
        Some(format!("receipts {total}"))
    }

    pub(crate) fn redactions(app: &TuiApp) -> Option<String> {
        Some(format!("redactions {}", app.metrics.redactions))
    }

    pub(crate) fn cached_tokens(app: &TuiApp) -> Option<String> {
        Some(format!(
            "cached {}",
            format_optional_u64(app.cost.cached_input_tokens)
        ))
    }

    pub(crate) fn cache_write_tokens(app: &TuiApp) -> Option<String> {
        Some(format!(
            "cache_write {}",
            format_optional_u64(app.cost.cache_write_input_tokens)
        ))
    }
}
