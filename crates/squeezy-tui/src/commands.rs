use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::time::SystemTime;

use squeezy_agent::{
    Agent, CalibrationSource, McpAccounting, ReviewerAuditEntry, SessionAccountingSnapshot,
    SkillsAccounting,
};
use squeezy_llm::{LimitSource, RequestTokenEstimate};
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
    // When spend spans more than the active model (switch / reroute / a
    // different-model subagent), mark the header model as the *active* one so
    // it doesn't read as the sole spender — the authoritative split is the
    // "By model" section below.
    let model_active_tag = if !metrics.model_ledger.is_empty() {
        format!(" {}", style::muted("(active)"))
    } else {
        String::new()
    };
    out.push_str(&format!(
        "  {} provider  {}   model {}{}   mode {}\n",
        style::accent("◉"),
        style::accent(snapshot.provider),
        style::accent(&snapshot.model),
        model_active_tag,
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
    if !metrics.model_ledger.is_empty() {
        out.push('\n');
        out.push_str(&style::header("By model"));
        out.push('\n');
        let mut buckets: Vec<&squeezy_core::ModelCostBucket> =
            metrics.model_ledger.iter().collect();
        // Highest spend first; stable tie-break on provider:model keeps the
        // output (and its tests) deterministic across runs.
        buckets.sort_by(|a, b| {
            b.total_usd_micros()
                .unwrap_or(0)
                .cmp(&a.total_usd_micros().unwrap_or(0))
                .then_with(|| {
                    (a.provider.as_str(), a.model.as_str())
                        .cmp(&(b.provider.as_str(), b.model.as_str()))
                })
        });
        for bucket in buckets {
            let active = bucket.provider == snapshot.provider && bucket.model == snapshot.model;
            let model_label = format!("{}:{}", bucket.provider, bucket.model);
            let styled_label = if active {
                format!(
                    "{} {}",
                    style::accent(&model_label),
                    style::muted("(active)")
                )
            } else {
                style::accent(&model_label)
            };
            // One row per non-empty origin so the main-vs-subagent split is
            // explicit; a model with both prints two rows under one label.
            for (scope, slot) in [("main", &bucket.main), ("subagent", &bucket.subagent)] {
                if !cost_snapshot_has_data(slot) {
                    continue;
                }
                out.push_str(&format!(
                    "  {} {} {} usd={} in={} out={} cache_r={} cache_w={}\n",
                    style::accent("◉"),
                    styled_label,
                    style::muted(scope),
                    format_cost_styled(slot),
                    style_optional_u64(slot.input_tokens),
                    style_optional_u64(slot.output_tokens),
                    style_optional_u64(slot.cached_input_tokens),
                    style_optional_u64(slot.cache_write_input_tokens),
                ));
            }
        }
        let totals = metrics.model_ledger.totals();
        out.push_str(&format!(
            "  {} total usd={} in={} out={} cache_r={} cache_w={}\n",
            style::accent("Σ"),
            format_cost_styled(&totals),
            style_optional_u64(totals.input_tokens),
            style_optional_u64(totals.output_tokens),
            style_optional_u64(totals.cached_input_tokens),
            style_optional_u64(totals.cache_write_input_tokens),
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
    out.push_str(&style::header("Token calibration"));
    out.push('\n');
    out.push_str(&format!(
        "  {} source  {}\n",
        style::accent("◎"),
        style::muted(snapshot.calibration_source.as_str()),
    ));
    let calibration_note = match snapshot.calibration_source {
        CalibrationSource::HardCodedDefault => {
            "token estimates use provider hard-coded defaults; run a session to warm the calibration"
        }
        CalibrationSource::CorruptFallback => {
            "calibration.json was malformed; check for file corruption on shared or network homes"
        }
        CalibrationSource::GlobalFile => {
            "estimates warmed from prior session data in calibration.json"
        }
        CalibrationSource::ResumedSession => "estimates warmed from this session's saved metadata",
    };
    out.push_str(&format!(
        "  {} {}\n",
        style::accent("ℹ"),
        style::muted(calibration_note)
    ));

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

    // Provenance: where the window came from and how much to trust it, plus the
    // previously-hidden effective reduction — so an exact user/provider value
    // reads differently from a 272K guess. Kept to one line in the common case.
    let est = &snapshot.transmitted_request;
    let mut source_line = format!(
        "  {} source: {} ({})",
        style::accent("◈"),
        style::accent(est.limit_source.as_str()),
        est.limit_confidence.as_str(),
    );
    if let (Some(raw), Some(effective)) = (
        est.context_window_tokens,
        est.effective_context_window_tokens,
    ) {
        source_line.push_str(&format!(
            "  ·  effective {} {}",
            style::accent(&style::group_thousands(effective)),
            style::muted(&format!(
                "({}% of {} −{} baseline)",
                est.effective_context_window_percent,
                style::group_thousands(raw),
                style::group_thousands(est.baseline_reserve_tokens),
            )),
        ));
    }
    source_line.push('\n');
    out.push_str(&source_line);
    if matches!(est.limit_source, LimitSource::SyntheticFallback) {
        out.push_str(&format!(
            "  {}\n",
            style::muted(
                "window is a generic estimate — set context_window for this model on the Models \
                 config page for accuracy",
            ),
        ));
    }
    if matches!(est.limit_source, LimitSource::ObservedBound)
        && let Some(ceiling) = est.observed_ceiling_tokens
    {
        out.push_str(&format!(
            "  {} {}\n",
            style::accent("◈"),
            style::muted(&format!(
                "clamped to {} after a provider context-overflow this session",
                style::group_thousands(ceiling),
            )),
        ));
    }
    if let Some(dev) = est.models_dev_window_tokens
        && Some(dev) != est.context_window_tokens
    {
        out.push_str(&format!(
            "  {} {}\n",
            style::accent("◈"),
            style::muted(&format!(
                "models.dev reports {}",
                style::group_thousands(dev)
            )),
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
    // Skill bodies (load_skill outputs) are carved out of the raw tool-output
    // total and reported as their own bucket; the remainder is the genuine
    // tool-output cost.
    let skill_tokens = approx(snapshot.conversation.skill_output_bytes);
    let tool_only_bytes = snapshot
        .conversation
        .tool_output_bytes
        .saturating_sub(snapshot.conversation.skill_output_bytes);
    let tool_tokens = approx(tool_only_bytes);
    // MCP tool schemas live in the per-request framing; carve them out of the
    // opaque system + framing remainder computed below. Lazy loading means the
    // live cost is the stub lines plus only the loaded full schemas.
    let mcp_tokens = approx(snapshot.mcp.in_context_bytes_total);
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
            "({} bytes from {} call(s){})",
            tool_only_bytes,
            snapshot.conversation.function_outputs,
            if skill_tokens > 0 {
                "; skill bodies carved out below"
            } else {
                ""
            },
        )),
    ));
    if skill_tokens > 0 {
        out.push_str(&format!(
            "  {} skills (loaded bodies):   ~{} tokens  {}\n",
            style::accent("◆"),
            style::accent_bold(&style::group_thousands(skill_tokens as u64)),
            style::muted(&format!(
                "({} bytes via load_skill; {} of {} skills loaded)",
                snapshot.conversation.skill_output_bytes,
                snapshot.skills.loaded,
                snapshot.skills.discovered,
            )),
        ));
    }
    if snapshot.mcp.in_context_bytes_total > 0 {
        out.push_str(&format!(
            "  {} mcp tool schemas:         ~{} tokens  {}\n",
            style::accent("◆"),
            style::accent_bold(&style::group_thousands(mcp_tokens as u64)),
            style::muted(&format!(
                "({} bytes live; {} tool(s) across {} server(s))",
                snapshot.mcp.in_context_bytes_total,
                snapshot.mcp.total_tools,
                snapshot.mcp.servers.len(),
            )),
        ));
    }
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
    let accounted = user_tokens
        + tool_tokens
        + skill_tokens
        + mcp_tokens
        + reasoning_tokens
        + image_tokens
        + attachment_tokens;
    let overhead_estimate = consumed.saturating_sub(accounted as u64);
    out.push_str(&format!(
        "  {} base request overhead:  ~{} tokens  {}\n",
        style::secondary("◇"),
        style::secondary(&style::group_thousands(overhead_estimate)),
        style::muted(
            "(remainder: Squeezy instructions, repo profile, built-in tool schemas, tool index, and estimation slack)",
        ),
    ));

    // Actionable per-source advice derived from the same per-source token
    // shares rendered above. Purely additive: the breakdown numbers are
    // unchanged; this block only suggests what to do about contributors the
    // user can directly influence. Deterministic — no model call, fixed thresholds.
    let recommendations = context_source_recommendations(&ContextSourceTokens {
        user: user_tokens as u64,
        tool_outputs: tool_tokens as u64,
        skills: skill_tokens as u64,
        mcp: mcp_tokens as u64,
        reasoning: reasoning_tokens as u64,
        image: image_tokens as u64,
        attachments: attachment_tokens as u64,
        overhead: overhead_estimate,
    });
    if !recommendations.is_empty() {
        out.push('\n');
        out.push_str(&style::header("Recommendations"));
        out.push('\n');
        for rec in &recommendations {
            out.push_str(&format!("  {} {}\n", style::warn("→"), rec));
        }
    }

    // Only surface the Skills/MCP block when there is something to show — an
    // empty inventory would just add noise (and height) to every /context.
    if snapshot.skills.discovered > 0 || !snapshot.mcp.servers.is_empty() {
        out.push('\n');
        format_skills_mcp_total(&mut out, &snapshot.skills, &snapshot.mcp);
        out.push('\n');
        format_skills_section(&mut out, &snapshot.skills);
        out.push('\n');
        format_mcp_section(&mut out, &snapshot.mcp);
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

/// Truncate `value` to `max` display characters, appending `…` when cut.
fn truncate_display(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut out: String = value.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Round bytes to the same 4-bytes/token estimate used across `/context`.
fn approx_tokens(bytes: usize) -> u64 {
    bytes.div_ceil(4) as u64
}

/// Render the combined "Skills + MCP" grand total that heads the two sections:
/// the in-context cost of skill metadata + loaded bodies plus MCP stubs +
/// loaded schemas, with the two subtotals broken out.
fn format_skills_mcp_total(out: &mut String, skills: &SkillsAccounting, mcp: &McpAccounting) {
    let skills_bytes = skills.metadata_bytes_total + skills.loaded_body_bytes_total;
    let mcp_bytes = mcp.in_context_bytes_total;
    out.push_str(&style::header("Skills + MCP"));
    out.push(' ');
    out.push_str(&style::muted(&format!(
        "(~{} tok in context = skills ~{} + mcp ~{})",
        style::group_thousands(approx_tokens(skills_bytes + mcp_bytes)),
        style::group_thousands(approx_tokens(skills_bytes)),
        style::group_thousands(approx_tokens(mcp_bytes)),
    )));
    out.push('\n');
}

/// Render the "Skills" section: every discovered skill split into its
/// always-present metadata cost and its body cost (counted when loaded),
/// with a `(loaded)` marker and the section subtotal in the header.
fn format_skills_section(out: &mut String, skills: &SkillsAccounting) {
    out.push_str(&style::header("Skills"));
    if skills.discovered == 0 {
        out.push('\n');
        out.push_str(&format!("  {}\n", style::muted("none discovered")));
        return;
    }
    let total_bytes = skills.metadata_bytes_total + skills.loaded_body_bytes_total;
    out.push(' ');
    out.push_str(&style::muted(&format!(
        "({} discovered, {} loaded · meta ~{} + bodies ~{} = ~{} tok)",
        skills.discovered,
        skills.loaded,
        style::group_thousands(approx_tokens(skills.metadata_bytes_total)),
        style::group_thousands(approx_tokens(skills.loaded_body_bytes_total)),
        style::group_thousands(approx_tokens(total_bytes)),
    )));
    out.push('\n');
    let name_width = skills
        .entries
        .iter()
        .map(|entry| entry.name.chars().count())
        .max()
        .unwrap_or(0);
    for entry in &skills.entries {
        let icon = if entry.loaded {
            style::accent("◆")
        } else {
            style::secondary("◇")
        };
        let pad = " ".repeat(name_width.saturating_sub(entry.name.chars().count()));
        let meta_tokens = approx_tokens(entry.metadata_bytes);
        let body_tokens = approx_tokens(entry.body_bytes);
        // Loaded: meta + body are both in context. Not loaded: only meta is
        // present; show the body as the cost a first load would add.
        let breakdown = if entry.loaded {
            format!(
                "meta ~{} + body ~{} = ~{} tok  {}",
                style::group_thousands(meta_tokens),
                style::group_thousands(body_tokens),
                style::accent_bold(&style::group_thousands(meta_tokens + body_tokens)),
                style::muted("(loaded)"),
            )
        } else {
            format!(
                "meta ~{} tok  {}",
                style::accent_bold(&style::group_thousands(meta_tokens)),
                style::muted(&format!(
                    "(+~{} if loaded)",
                    style::group_thousands(body_tokens)
                )),
            )
        };
        out.push_str(&format!(
            "  {} {}{}  {}\n",
            icon, entry.name, pad, breakdown
        ));
    }
}

/// Render the "MCPs" section: connected servers with live status and a
/// per-tool split of the lazy stub cost from the full-schema (first-load)
/// cost, with per-server and section subtotals.
fn format_mcp_section(out: &mut String, mcp: &McpAccounting) {
    out.push_str(&style::header("MCPs"));
    if mcp.servers.is_empty() {
        out.push('\n');
        out.push_str(&format!("  {}\n", style::muted("none configured")));
        return;
    }
    out.push(' ');
    out.push_str(&style::muted(&format!(
        "({} server(s), {} tool(s) · stubs ~{} + loaded ~{} = ~{} tok)",
        mcp.servers.len(),
        mcp.total_tools,
        style::group_thousands(approx_tokens(mcp.stub_bytes_total)),
        style::group_thousands(approx_tokens(mcp.loaded_full_bytes_total)),
        style::group_thousands(approx_tokens(mcp.in_context_bytes_total)),
    )));
    out.push('\n');
    for server in &mcp.servers {
        out.push_str(&format!(
            "  {} {}  {}  ~{} tok\n",
            style::accent("●"),
            style::accent(&server.name),
            style::muted(&server.status),
            style::accent_bold(&style::group_thousands(approx_tokens(
                server.in_context_bytes
            ))),
        ));
        for tool in &server.tools {
            let stub_tokens = approx_tokens(tool.stub_bytes);
            let full_tokens = approx_tokens(tool.full_bytes);
            // Loaded tools carry stub + full; lazy-deferred tools carry only the
            // stub, with the full schema shown as the first-load cost increase.
            let cost = if tool.loaded {
                if tool.stub_bytes > 0 {
                    format!(
                        "stub ~{} + schema ~{} = ~{} tok  {}",
                        style::group_thousands(stub_tokens),
                        style::group_thousands(full_tokens),
                        style::group_thousands(stub_tokens + full_tokens),
                        style::muted("(loaded)"),
                    )
                } else {
                    format!(
                        "schema ~{} tok  {}",
                        style::group_thousands(full_tokens),
                        style::muted("(loaded)"),
                    )
                }
            } else {
                format!(
                    "stub ~{} tok  {}",
                    style::group_thousands(stub_tokens),
                    style::muted(&format!(
                        "(+~{} on first load)",
                        style::group_thousands(full_tokens)
                    )),
                )
            };
            out.push_str(&format!(
                "      {} {}  {}  {}\n",
                style::secondary("-"),
                tool.name,
                style::muted(&truncate_display(&tool.description, 48)),
                cost,
            ));
        }
    }
}

/// Per-source token estimates fed into [`context_source_recommendations`].
/// Mirrors the buckets rendered under "Consumption by source"; fixed request
/// overhead stays in the denominator but is not itself a recommendation target.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ContextSourceTokens {
    pub user: u64,
    pub tool_outputs: u64,
    pub skills: u64,
    pub mcp: u64,
    pub reasoning: u64,
    pub image: u64,
    pub attachments: u64,
    pub overhead: u64,
}

/// Deterministic, data-only cut recommendations derived from the existing
/// per-source token shares. No model call, no randomness: the same shape
/// always yields the same ordered advice. Returns an empty vec when the
/// context is effectively empty (nothing actionable to say yet).
///
/// The rule is intentionally simple and explainable: identify the single
/// largest actionable source by share, and — when it crosses a meaningful
/// fraction of the assembled request — emit a targeted suggestion plus any
/// secondary callouts that are individually large enough to act on.
pub(crate) fn context_source_recommendations(tokens: &ContextSourceTokens) -> Vec<String> {
    let total = tokens.user
        + tokens.tool_outputs
        + tokens.skills
        + tokens.mcp
        + tokens.reasoning
        + tokens.image
        + tokens.attachments
        + tokens.overhead;
    // Nothing assembled yet (fresh session): no actionable advice.
    if total == 0 {
        return Vec::new();
    }

    let pct = |value: u64| ((value as f64 / total as f64) * 100.0).round() as u64;
    // Ordered by descending share so "largest actionable" ties break
    // deterministically toward the bucket the user is most likely to recognize
    // as actionable. Base request overhead is intentionally excluded from this
    // list: it is mostly Squeezy-owned instructions and tool-advertising cost,
    // not a direct user cleanup knob.
    let sources: [(&str, u64, &str); 7] = [
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
            "skills",
            tokens.skills,
            "unload skills you are done with (they reload on demand) or use metadata-only skill mode",
        ),
        (
            "mcp",
            tokens.mcp,
            "disable unused MCP servers to shrink the advertised tool schemas",
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
        recs.push(format!(
            "largest actionable: {name} {}% → {action}",
            pct(value)
        ));
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
    output.push_str(&format!(
        " window_source={} window_confidence={}",
        estimate.limit_source.as_str(),
        estimate.limit_confidence.as_str(),
    ));
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

/// Whether a per-model ledger slot carries any spend worth a row — used to
/// suppress an empty main/subagent sub-row in the "By model" drill.
fn cost_snapshot_has_data(cost: &squeezy_core::CostSnapshot) -> bool {
    cost.estimated_usd_micros.unwrap_or(0) > 0
        || cost.input_tokens.unwrap_or(0) > 0
        || cost.output_tokens.unwrap_or(0) > 0
        || cost.cached_input_tokens.unwrap_or(0) > 0
        || cost.cache_write_input_tokens.unwrap_or(0) > 0
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
