use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::time::SystemTime;

use squeezy_agent::{Agent, ReviewerAuditEntry, SessionAccountingSnapshot};
use squeezy_llm::RequestTokenEstimate;
use squeezy_store::parse_bug_report_section;

use crate::commands_style as style;

pub(crate) fn parse_report_preview_args(
    agent: &Agent,
    rest: &str,
) -> std::result::Result<(String, BTreeSet<String>), String> {
    let mut session_id = None;
    let mut excluded_sections = BTreeSet::new();
    for part in rest.split_whitespace() {
        if let Some(raw) = part.strip_prefix("exclude=") {
            for section in raw.split(',').filter(|section| !section.trim().is_empty()) {
                let Some(parsed) = parse_bug_report_section(section) else {
                    return Err(format!("unknown report section {section:?}"));
                };
                excluded_sections.insert(parsed.to_string());
            }
        } else if session_id.is_none() {
            session_id = Some(part.to_string());
        } else {
            return Err(
                "usage: /report [session_id] [exclude=a,b] | /report send | /report cancel"
                    .to_string(),
            );
        }
    }
    let session_id = session_id
        .or_else(|| agent.session_id())
        .ok_or_else(|| "usage: /report <session_id> [exclude=a,b]".to_string())?;
    Ok((session_id, excluded_sections))
}

pub(crate) fn format_cost_command(snapshot: &SessionAccountingSnapshot) -> String {
    let cost = &snapshot.cost;
    let metrics = &snapshot.metrics;
    let mut out = String::with_capacity(1024);

    // Header keeps the literal "Cost accounting" prefix (callers and
    // scripts may grep for it); ANSI wrappers add the bold accent color.
    out.push_str(&style::header("Cost accounting"));
    out.push('\n');
    out.push_str(&format!(
        "  {} session   {}\n",
        style::accent("◷"),
        style::muted(snapshot.session_id.as_deref().unwrap_or("-")),
    ));
    out.push_str(&format!(
        "  {} provider  {}   model {}   mode {}\n",
        style::accent("◉"),
        style::accent(snapshot.provider),
        style::accent(&snapshot.model),
        style::accent(snapshot.mode.as_str()),
    ));
    out.push_str(&format!(
        "  {} estimated_usd={} {}\n",
        style::accent("$"),
        format_cost_styled(cost),
        style::muted("(estimated from provider-reported usage and local pricing metadata)"),
    ));

    out.push('\n');
    out.push_str(&style::header("Tokens (provider-reported)"));
    out.push('\n');
    out.push_str(&format!(
        "  {} provider_tokens input={} output={} reasoning={} cached_input={} cache_write_input={}\n",
        style::accent("↕"),
        style_optional_u64(cost.input_tokens),
        style_optional_u64(cost.output_tokens),
        style_optional_u64(cost.reasoning_output_tokens),
        style_optional_u64(cost.cached_input_tokens),
        style_optional_u64(cost.cache_write_input_tokens),
    ));

    let tool_activity = metrics.tool_calls
        + metrics.tool_successes
        + metrics.tool_errors
        + metrics.tool_denials
        + metrics.tool_cancellations
        + metrics.budget_denials;
    if tool_activity > 0 {
        out.push('\n');
        out.push_str(&style::header("Tools"));
        out.push('\n');
        out.push_str(&format!(
            "  {} tools calls={} successes={} errors={} denials={} cancellations={} budget_denials={}\n",
            style::accent("⚙"),
            style_u64(metrics.tool_calls),
            style_u64(metrics.tool_successes),
            style_u64_emphasize_nonzero_err(metrics.tool_errors),
            style_u64(metrics.tool_denials),
            style_u64(metrics.tool_cancellations),
            style_u64_emphasize_nonzero_err(metrics.budget_denials),
        ));
    }
    if metrics.subagent_calls > 0 {
        out.push('\n');
        out.push_str(&style::header("Subagents"));
        out.push('\n');
        out.push_str(&format!(
            "  {} subagents calls={} failures={} estimated_usd={} input={} output={} tool_calls={} budget_denials={}\n",
            style::accent("⚙"),
            style_u64(metrics.subagent_calls),
            style_u64_emphasize_nonzero_err(metrics.subagent_failures),
            format_cost_styled(&metrics.subagent_provider),
            style_optional_u64(metrics.subagent_provider.input_tokens),
            style_optional_u64(metrics.subagent_provider.output_tokens),
            style_u64(metrics.subagent_tool_calls),
            style_u64_emphasize_nonzero_err(metrics.subagent_budget_denials),
        ));
    }
    let receipt_total = metrics.receipt_stub_hits + metrics.negative_receipt_hits;
    let spill_total = metrics.spill_writes + metrics.spill_reads;
    let io_activity = metrics.bytes_read
        + metrics.files_scanned
        + metrics.matches_returned
        + metrics.model_output_bytes
        + metrics.subagent_bytes_read
        + metrics.subagent_files_scanned
        + metrics.subagent_model_output_bytes;
    if receipt_total + spill_total + io_activity > 0 {
        out.push('\n');
        out.push_str(&style::header("Receipts · Spills · I/O"));
        out.push('\n');
    }
    if receipt_total > 0 {
        out.push_str(&format!(
            "  {} receipts stub_hits={} negative_stub_hits={} total_hits={}\n",
            style::accent("⤿"),
            style_u64(metrics.receipt_stub_hits),
            style_u64(metrics.negative_receipt_hits),
            style_u64(receipt_total),
        ));
    }
    if spill_total > 0 {
        out.push_str(&format!(
            "  {} spills writes={} reads={}\n",
            style::accent("⇅"),
            style_u64(metrics.spill_writes),
            style_u64(metrics.spill_reads),
        ));
    }
    if io_activity > 0 {
        out.push_str(&format!(
            "  {} io bytes_read={} files_scanned={} matches_returned={} model_output_bytes={} subagent_bytes_read={} subagent_files_scanned={} subagent_model_output_bytes={}\n",
            style::accent("⇆"),
            style_u64(metrics.bytes_read),
            style_u64(metrics.files_scanned),
            style_u64(metrics.matches_returned),
            style_u64(metrics.model_output_bytes),
            style_u64(metrics.subagent_bytes_read),
            style_u64(metrics.subagent_files_scanned),
            style_u64(metrics.subagent_model_output_bytes),
        ));
    }
    if snapshot.redactions > 0 {
        out.push_str(&format!(
            "  {} redactions={}\n",
            style::accent("✦"),
            style_u64(snapshot.redactions),
        ));
    }

    out.push('\n');
    out.push_str(&style::muted(
        "accuracy=provider token counters are provider-reported when available; USD is an estimate, not a billing authority.",
    ));
    out
}

pub(crate) fn format_context_command(snapshot: &SessionAccountingSnapshot) -> String {
    let mut out = String::with_capacity(1536);

    // Header literal stays "Context window" so existing transcript
    // grep tests and any external scrapers continue to match; the
    // ANSI wrapper just paints it.
    out.push_str(&style::header("Context window"));
    out.push('\n');
    let consumed = snapshot.transmitted_request.input_tokens;
    let window = snapshot.transmitted_request.context_window_tokens;
    match window {
        Some(window) if window > 0 => {
            let remaining = window.saturating_sub(consumed);
            let used_pct = (consumed as f64 / window as f64) * 100.0;
            let remaining_pct = 100.0 - used_pct;
            out.push_str(&format!(
                "  {} consumed:  {} tokens ({} of {} window)\n",
                style::accent("▮"),
                style::accent_bold(&style::group_thousands(consumed)),
                style::accent(&format!("{used_pct:.1}%")),
                style::accent(&style::group_thousands(window)),
            ));
            // Headroom percentage is the single most important visual
            // indicator — green when safe, yellow when getting close,
            // red when nearly full.
            let headroom_label = format!("{remaining_pct:.1}% headroom");
            out.push_str(&format!(
                "  {} remaining: {} tokens ({})\n",
                style::accent("▯"),
                style::accent_bold(&style::group_thousands(remaining)),
                style::headroom(remaining_pct, &headroom_label),
            ));
        }
        _ => {
            out.push_str(&format!(
                "  {} consumed:  {} tokens {}\n",
                style::accent("▮"),
                style::accent_bold(&style::group_thousands(consumed)),
                style::muted("(context window unknown for this model)"),
            ));
        }
    }
    if let Some(max_output) = snapshot.transmitted_request.max_output_tokens {
        out.push_str(&format!(
            "  {} max_output_reserve: {} tokens\n",
            style::accent("◌"),
            style::accent(&style::group_thousands(max_output)),
        ));
    }

    // Per-source breakdown derived from existing accounting. Token
    // estimates use the rough 4-bytes/token heuristic for text/tool
    // bytes — same accuracy class as the consumed total above (both
    // are deterministic local estimates of assembled request content).
    out.push('\n');
    out.push_str(&style::header("Consumption by source"));
    out.push('\n');
    let approx = |bytes: usize| bytes.div_ceil(4);
    let user_tokens = approx(snapshot.conversation.text_bytes);
    let tool_tokens = approx(snapshot.conversation.tool_output_bytes);
    let reasoning_tokens = approx(snapshot.conversation.reasoning_bytes);
    let image_tokens = approx(snapshot.conversation.image_bytes);
    let attachment_tokens = approx(snapshot.attachments.stored_bytes);
    out.push_str(&format!(
        "  {} user + assistant text:    ~{} tokens  {}\n",
        style::accent("◆"),
        style::accent_bold(&style::group_thousands(user_tokens as u64)),
        style::muted(&format!(
            "({} bytes; {} user / {} assistant items)",
            snapshot.conversation.text_bytes,
            snapshot.conversation.user_text,
            snapshot.conversation.assistant_text,
        )),
    ));
    out.push_str(&format!(
        "  {} tool call outputs:        ~{} tokens  {}\n",
        style::accent("◆"),
        style::accent_bold(&style::group_thousands(tool_tokens as u64)),
        style::muted(&format!(
            "({} bytes from {} call(s); MCP / skill / internal split needs deeper accounting)",
            snapshot.conversation.tool_output_bytes, snapshot.conversation.function_outputs,
        )),
    ));
    if snapshot.conversation.reasoning_bytes > 0 {
        out.push_str(&format!(
            "  {} reasoning content:        ~{} tokens  {}\n",
            style::accent("◆"),
            style::accent(&style::group_thousands(reasoning_tokens as u64)),
            style::muted(&format!(
                "({} bytes across {} item(s))",
                snapshot.conversation.reasoning_bytes, snapshot.conversation.reasoning_items,
            )),
        ));
    }
    if snapshot.conversation.image_bytes > 0 {
        out.push_str(&format!(
            "  {} image content:            ~{} tokens  {}\n",
            style::accent("◆"),
            style::accent(&style::group_thousands(image_tokens as u64)),
            style::muted(&format!(
                "({} bytes across {} item(s))",
                snapshot.conversation.image_bytes, snapshot.conversation.image_items,
            )),
        ));
    }
    if snapshot.attachments.stored_bytes > 0 {
        out.push_str(&format!(
            "  {} attached context:         ~{} tokens  {}\n",
            style::accent("◆"),
            style::accent(&style::group_thousands(attachment_tokens as u64)),
            style::muted(&format!(
                "({} bytes; {} active of {} total)",
                snapshot.attachments.stored_bytes,
                snapshot.attachments.active,
                snapshot.attachments.total,
            )),
        ));
    }
    let accounted = user_tokens + tool_tokens + reasoning_tokens + image_tokens + attachment_tokens;
    let system_estimate = consumed.saturating_sub(accounted as u64);
    out.push_str(&format!(
        "  {} system prompt + framing: ~{} tokens  {}\n",
        style::secondary("◇"),
        style::secondary(&style::group_thousands(system_estimate)),
        style::muted(
            "(consumed minus the above; covers system prompt, tool schemas, and per-request framing)",
        ),
    ));

    // Actionable per-source advice derived from the same per-source token
    // shares rendered above. Purely additive: the breakdown numbers are
    // unchanged; this block only suggests what to do about the largest
    // contributors. Deterministic — no model call, fixed thresholds.
    let recommendations = context_source_recommendations(&ContextSourceTokens {
        user: user_tokens as u64,
        tool_outputs: tool_tokens as u64,
        reasoning: reasoning_tokens as u64,
        image: image_tokens as u64,
        attachments: attachment_tokens as u64,
        system: system_estimate,
    });
    if !recommendations.is_empty() {
        out.push('\n');
        out.push_str(&style::header("Recommendations"));
        out.push('\n');
        for rec in &recommendations {
            out.push_str(&format!("  {} {}\n", style::warn("→"), rec));
        }
    }

    out.push('\n');
    out.push_str(&style::header("Session"));
    out.push('\n');
    out.push_str(&format!(
        "  {} session={}\n",
        style::accent("●"),
        style::muted(snapshot.session_id.as_deref().unwrap_or("-")),
    ));
    out.push_str(&format!(
        "  {} provider={} model={} mode={}\n",
        style::accent("●"),
        style::accent(snapshot.provider),
        style::accent(&snapshot.model),
        style::accent(snapshot.mode.as_str()),
    ));
    let response_state = if snapshot.store_responses {
        if snapshot.previous_response_id.is_some() {
            "store_responses=true previous_response_id=present"
        } else {
            "store_responses=true previous_response_id=absent"
        }
    } else {
        "store_responses=false"
    };
    out.push_str(&format!("  {} {}\n", style::accent("◐"), response_state));
    if snapshot.provider_stored_context_active() {
        out.push_str(&format!(
            "  {} {}\n",
            style::secondary("⚠"),
            style::muted(
                "provider_stored_context=active; exact provider-side current-window use is unknown — compare transmitted request with the local full-history estimate",
            ),
        ));
    }
    out.push_str(&format!(
        "  {} turns={} provider_tokens input={} output={} reasoning={} cached_input={} cache_write_input={}\n",
        style::accent("↻"),
        style_u64(snapshot.metrics.turns),
        style_optional_u64(snapshot.cost.input_tokens),
        style_optional_u64(snapshot.cost.output_tokens),
        style_optional_u64(snapshot.cost.reasoning_output_tokens),
        style_optional_u64(snapshot.cost.cached_input_tokens),
        style_optional_u64(snapshot.cost.cache_write_input_tokens),
    ));
    if snapshot.metrics.routed_to_cheap_turns > 0
        || snapshot.metrics.escalated_to_parent_turns > 0
        || snapshot.metrics.routing_judge_usd_micros > 0
        || snapshot.metrics.routing_estimated_net_savings_usd_micros != 0
    {
        out.push_str(&format!(
            "  routing routed_turns={} escalated={} judge=${:.6} net_savings=${:.6}\n",
            snapshot.metrics.routed_to_cheap_turns,
            snapshot.metrics.escalated_to_parent_turns,
            snapshot.metrics.routing_judge_usd_micros as f64 / 1_000_000.0,
            snapshot.metrics.routing_estimated_net_savings_usd_micros as f64 / 1_000_000.0,
        ));
    }

    out.push('\n');
    out.push_str(&style::header("Volume"));
    out.push('\n');
    out.push_str(&format!(
        "  {} tools calls={} results={} receipt_hits={} spill_writes={} spill_reads={} budget_denials={}\n",
        style::accent("⚙"),
        style_u64(snapshot.metrics.tool_calls),
        style_u64(
            snapshot.metrics.tool_successes
                + snapshot.metrics.tool_errors
                + snapshot.metrics.tool_denials
                + snapshot.metrics.tool_cancellations,
        ),
        style_u64(snapshot.metrics.receipt_stub_hits + snapshot.metrics.negative_receipt_hits),
        style_u64(snapshot.metrics.spill_writes),
        style_u64(snapshot.metrics.spill_reads),
        style_u64_emphasize_nonzero_err(snapshot.metrics.budget_denials),
    ));
    out.push_str(&format!(
        "  {} subagents calls={} failures={} tool_calls={} bytes_read={} files_scanned={} model_output_bytes={} budget_denials={}\n",
        style::accent("⚙"),
        style_u64(snapshot.metrics.subagent_calls),
        style_u64_emphasize_nonzero_err(snapshot.metrics.subagent_failures),
        style_u64(snapshot.metrics.subagent_tool_calls),
        style_u64(snapshot.metrics.subagent_bytes_read),
        style_u64(snapshot.metrics.subagent_files_scanned),
        style_u64(snapshot.metrics.subagent_model_output_bytes),
        style_u64_emphasize_nonzero_err(snapshot.metrics.subagent_budget_denials),
    ));

    out.push('\n');
    out.push_str(&style::header("Request estimates"));
    out.push_str("\n  ");
    out.push_str(&style::accent("→"));
    out.push(' ');
    out.push_str(&format_request_estimate(
        "transmitted_request",
        &snapshot.transmitted_request,
    ));
    out.push_str("\n  ");
    out.push_str(&style::accent("→"));
    out.push(' ');
    out.push_str(&format_request_estimate(
        "local_full_history",
        &snapshot.full_history_request,
    ));
    out.push_str("\n\n");
    out.push_str(&style::muted(
        "accuracy: token counts above are deterministic local estimates of assembled request content; per-source token attribution at the MCP / internal-tool / skill level requires deeper instrumentation (see squeezy-rw0i).",
    ));
    out
}

/// Per-source token estimates fed into [`context_source_recommendations`].
/// Mirrors the buckets rendered under "Consumption by source" so the advice
/// stays consistent with the breakdown the user already sees.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ContextSourceTokens {
    pub user: u64,
    pub tool_outputs: u64,
    pub reasoning: u64,
    pub image: u64,
    pub attachments: u64,
    pub system: u64,
}

/// Deterministic, data-only cut recommendations derived from the existing
/// per-source token shares. No model call, no randomness: the same shape
/// always yields the same ordered advice. Returns an empty vec when the
/// context is effectively empty (nothing actionable to say yet).
///
/// The rule is intentionally simple and explainable: identify the single
/// largest source by share, and — when it crosses a meaningful fraction of
/// the assembled request — emit a targeted suggestion plus any secondary
/// callouts (history/reasoning) that are individually large enough to act on.
pub(crate) fn context_source_recommendations(tokens: &ContextSourceTokens) -> Vec<String> {
    let total = tokens.user
        + tokens.tool_outputs
        + tokens.reasoning
        + tokens.image
        + tokens.attachments
        + tokens.system;
    // Nothing assembled yet (fresh session): no actionable advice.
    if total == 0 {
        return Vec::new();
    }

    let pct = |value: u64| ((value as f64 / total as f64) * 100.0).round() as u64;
    // Ordered by descending share so "largest" ties break deterministically
    // toward the bucket the user is most likely to recognize as actionable.
    let sources: [(&str, u64, &str); 6] = [
        (
            "tool_outputs",
            tokens.tool_outputs,
            "narrow reads (read_slice / signature spans), prefer grep counts, or enable output dedup",
        ),
        (
            "history",
            tokens.user,
            "run /compact to summarize older turns",
        ),
        (
            "reasoning",
            tokens.reasoning,
            "lower reasoning effort for routine turns",
        ),
        (
            "attachments",
            tokens.attachments,
            "drop stale attachments with /context or detach unused files",
        ),
        (
            "images",
            tokens.image,
            "downscale images or remove ones no longer referenced",
        ),
        (
            "system",
            tokens.system,
            "trim custom system prompt / tool schemas if customizable",
        ),
    ];

    let mut recs = Vec::new();
    // Largest source: only worth surfacing once it dominates a real share of
    // the request (below this it's noise and any advice is premature).
    const LARGEST_THRESHOLD_PCT: u64 = 25;
    if let Some((name, value, action)) = sources
        .iter()
        .copied()
        .max_by_key(|(_, value, _)| *value)
        .filter(|(_, value, _)| pct(*value) >= LARGEST_THRESHOLD_PCT)
    {
        recs.push(format!("largest: {name} {}% → {action}", pct(value)));
    }

    // Secondary callouts: any *other* source that is independently large
    // enough to act on, so a 30% history slice isn't hidden behind a 40%
    // tool-output line. Reported in the fixed source order above.
    const SECONDARY_THRESHOLD_PCT: u64 = 30;
    let largest_name = recs
        .first()
        .map(|_| {
            sources
                .iter()
                .copied()
                .max_by_key(|(_, value, _)| *value)
                .map(|(name, _, _)| name)
                .unwrap_or("")
        })
        .unwrap_or("");
    for (name, value, action) in sources.iter().copied() {
        if name == largest_name {
            continue;
        }
        if pct(value) >= SECONDARY_THRESHOLD_PCT {
            recs.push(format!("{name} {}% → {action}", pct(value)));
        }
    }

    recs
}

fn format_request_estimate(label: &str, estimate: &RequestTokenEstimate) -> String {
    let mut output = format!(
        "{} input_tokens={} tokenizer={} accuracy={}",
        label,
        estimate.input_tokens,
        estimate.tokenizer.as_str(),
        if estimate.estimated {
            "estimated"
        } else {
            "exact"
        }
    );
    if let Some(context_window) = estimate.context_window_tokens {
        output.push_str(&format!(" context_window={context_window}"));
    } else {
        output.push_str(" context_window=unknown");
    }
    if let Some(max_output) = estimate.max_output_tokens {
        output.push_str(&format!(" max_output_reserve={max_output}"));
    } else {
        output.push_str(" max_output_reserve=unknown");
    }
    if let Some(input_budget) = estimate.input_budget_tokens {
        output.push_str(&format!(" input_budget={input_budget}"));
    } else {
        output.push_str(" input_budget=unknown");
    }
    if let Some(remaining) = estimate.remaining_input_tokens {
        output.push_str(&format!(" remaining_input_budget={remaining}"));
    } else {
        output.push_str(" remaining_input_budget=unknown");
    }
    if let Some(percent) = estimate.used_input_percent_x100 {
        output.push_str(&format!(" used={}", format_percent_x100(percent)));
    } else {
        output.push_str(" used=unknown");
    }
    output
}

fn format_percent_x100(value: u32) -> String {
    format!("{}.{:02}%", value / 100, value % 100)
}

pub(crate) fn format_optional_u64(value: Option<u64>) -> String {
    value.map_or("-".to_string(), |value| value.to_string())
}

pub(crate) fn format_cost(cost: &squeezy_core::CostSnapshot) -> String {
    cost.estimated_usd_micros.map_or("-".to_string(), |value| {
        format!("${:.6}", value as f64 / 1_000_000.0)
    })
}

/// `format_optional_u64` plus theme styling: real numbers pop in
/// accent, `-` placeholders dim to muted. Kept ungrouped so the
/// long `key=value` rows stay scannable as columns.
fn style_optional_u64(value: Option<u64>) -> String {
    match value {
        Some(v) => style::accent(&v.to_string()),
        None => style::muted("-"),
    }
}

/// Plain `u64` rendered with accent color when non-zero and muted
/// when zero, so a wall of zero counters fades into the background.
fn style_u64(value: u64) -> String {
    if value == 0 {
        style::muted("0")
    } else {
        style::accent(&value.to_string())
    }
}

/// Same as `style_u64` but paints non-zero values in the error color.
/// Used for things like `errors=`, `failures=`, `budget_denials=` —
/// counters whose *non-zeroness* is itself a signal.
fn style_u64_emphasize_nonzero_err(value: u64) -> String {
    if value == 0 {
        style::muted("0")
    } else {
        style::err(&value.to_string())
    }
}

/// Cost formatted with accent color when present, muted dash when not.
fn format_cost_styled(cost: &squeezy_core::CostSnapshot) -> String {
    match cost.estimated_usd_micros {
        Some(value) => style::accent(&format!("${:.6}", value as f64 / 1_000_000.0)),
        None => style::muted("-"),
    }
}

pub(crate) fn format_reviewer_command(entries: &[ReviewerAuditEntry], now: SystemTime) -> String {
    if entries.is_empty() {
        return "AI reviewer audit\nno auto-decisions recorded this session.".to_string();
    }
    let mut out = String::with_capacity(64 + entries.len() * 128);
    write!(
        &mut out,
        "AI reviewer audit\n{} recent decision(s); newest first.",
        entries.len()
    )
    .expect("writing to String cannot fail");
    for entry in entries.iter().rev() {
        let age = format_audit_age(now, entry.recorded_at);
        write!(
            &mut out,
            "\n{age} turn={turn} {verdict} {capability} {tool} target={target}\n    reason: {reason}",
            age = age,
            turn = entry.turn_id,
            verdict = entry.verdict.as_str(),
            capability = entry.capability.as_str(),
            tool = entry.tool_name,
            target = entry.target,
            reason = entry.reason,
        )
        .expect("writing to String cannot fail");
    }
    out
}

fn format_audit_age(now: SystemTime, when: SystemTime) -> String {
    let elapsed = now.duration_since(when).unwrap_or_default();
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs:>3}s")
    } else if secs < 3600 {
        format!("{:>3}m", secs / 60)
    } else if secs < 86_400 {
        format!("{:>3}h", secs / 3600)
    } else {
        format!("{:>3}d", secs / 86_400)
    }
}
