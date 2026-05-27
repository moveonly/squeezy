use std::collections::BTreeSet;
use std::time::SystemTime;

use squeezy_agent::{Agent, ReviewerAuditEntry, SessionAccountingSnapshot};
use squeezy_llm::RequestTokenEstimate;
use squeezy_store::parse_bug_report_section;

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
    let mut lines: Vec<String> = Vec::with_capacity(12);
    lines.push("Cost accounting".to_string());
    lines.push(format!(
        "session={}",
        snapshot.session_id.as_deref().unwrap_or("-")
    ));
    lines.push(format!(
        "provider={} model={} mode={}",
        snapshot.provider,
        snapshot.model,
        snapshot.mode.as_str(),
    ));
    lines.push(format!(
        "estimated_usd={} (estimated from provider-reported usage and local pricing metadata)",
        format_cost(cost),
    ));
    lines.push(format!(
        "provider_tokens input={} output={} reasoning={} cached_input={} cache_write_input={}",
        format_optional_u64(cost.input_tokens),
        format_optional_u64(cost.output_tokens),
        format_optional_u64(cost.reasoning_output_tokens),
        format_optional_u64(cost.cached_input_tokens),
        format_optional_u64(cost.cache_write_input_tokens),
    ));
    let tool_activity = metrics.tool_calls
        + metrics.tool_successes
        + metrics.tool_errors
        + metrics.tool_denials
        + metrics.tool_cancellations
        + metrics.budget_denials;
    if tool_activity > 0 {
        lines.push(format!(
            "tools calls={} successes={} errors={} denials={} cancellations={} budget_denials={}",
            metrics.tool_calls,
            metrics.tool_successes,
            metrics.tool_errors,
            metrics.tool_denials,
            metrics.tool_cancellations,
            metrics.budget_denials,
        ));
    }
    if metrics.subagent_calls > 0 {
        lines.push(format!(
            "subagents calls={} failures={} estimated_usd={} input={} output={} tool_calls={} budget_denials={}",
            metrics.subagent_calls,
            metrics.subagent_failures,
            format_cost(&metrics.subagent_provider),
            format_optional_u64(metrics.subagent_provider.input_tokens),
            format_optional_u64(metrics.subagent_provider.output_tokens),
            metrics.subagent_tool_calls,
            metrics.subagent_budget_denials,
        ));
    }
    let receipt_total = metrics.receipt_stub_hits + metrics.negative_receipt_hits;
    if receipt_total > 0 {
        lines.push(format!(
            "receipts stub_hits={} negative_stub_hits={} total_hits={}",
            metrics.receipt_stub_hits, metrics.negative_receipt_hits, receipt_total,
        ));
    }
    if metrics.spill_writes + metrics.spill_reads > 0 {
        lines.push(format!(
            "spills writes={} reads={}",
            metrics.spill_writes, metrics.spill_reads,
        ));
    }
    let io_activity = metrics.bytes_read
        + metrics.files_scanned
        + metrics.matches_returned
        + metrics.model_output_bytes
        + metrics.subagent_bytes_read
        + metrics.subagent_files_scanned
        + metrics.subagent_model_output_bytes;
    if io_activity > 0 {
        lines.push(format!(
            "io bytes_read={} files_scanned={} matches_returned={} model_output_bytes={} subagent_bytes_read={} subagent_files_scanned={} subagent_model_output_bytes={}",
            metrics.bytes_read,
            metrics.files_scanned,
            metrics.matches_returned,
            metrics.model_output_bytes,
            metrics.subagent_bytes_read,
            metrics.subagent_files_scanned,
            metrics.subagent_model_output_bytes,
        ));
    }
    if snapshot.redactions > 0 {
        lines.push(format!("redactions={}", snapshot.redactions));
    }
    lines.push(
        "accuracy=provider token counters are provider-reported when available; USD is an estimate, not a billing authority."
            .to_string(),
    );
    lines.join("\n")
}

pub(crate) fn format_context_command(snapshot: &SessionAccountingSnapshot) -> String {
    let response_state = if snapshot.store_responses {
        if snapshot.previous_response_id.is_some() {
            "store_responses=true previous_response_id=present"
        } else {
            "store_responses=true previous_response_id=absent"
        }
    } else {
        "store_responses=false"
    };
    let provider_gap = if snapshot.provider_stored_context_active() {
        "provider_stored_context=active; exact provider-side current-window use is unknown, so compare transmitted request with the local full-history estimate"
    } else {
        "provider_stored_context=inactive"
    };
    format!(
        "Context accounting\n\
session={}\n\
provider={} model={} mode={}\n\
response_state={}\n\
{}\n\
completed_turns={} provider_tokens input={} output={} reasoning={} cached_input={} cache_write_input={}\n\
transcript items={} user={} assistant={} system={} bytes={}\n\
local_history items={} user_text={} assistant_text={} function_calls={} function_outputs={} text_bytes={} tool_output_bytes={}\n\
attached_context total={} active={} removed={} unsupported={} stored_bytes={} redactions={}\n\
tool_volume calls={} results={} receipt_hits={} spill_writes={} spill_reads={} budget_denials={}\n\
subagent_volume calls={} failures={} tool_calls={} bytes_read={} files_scanned={} model_output_bytes={} budget_denials={}\n\
{}\n\
{}\n\
accuracy=context tokens are deterministic local estimates of assembled request content; percentages and remaining input budget are shown only when a model context limit is known.",
        snapshot.session_id.as_deref().unwrap_or("-"),
        snapshot.provider,
        snapshot.model,
        snapshot.mode.as_str(),
        response_state,
        provider_gap,
        snapshot.metrics.turns,
        format_optional_u64(snapshot.cost.input_tokens),
        format_optional_u64(snapshot.cost.output_tokens),
        format_optional_u64(snapshot.cost.reasoning_output_tokens),
        format_optional_u64(snapshot.cost.cached_input_tokens),
        format_optional_u64(snapshot.cost.cache_write_input_tokens),
        snapshot.transcript.items,
        snapshot.transcript.user,
        snapshot.transcript.assistant,
        snapshot.transcript.system,
        snapshot.transcript.bytes,
        snapshot.conversation.items,
        snapshot.conversation.user_text,
        snapshot.conversation.assistant_text,
        snapshot.conversation.function_calls,
        snapshot.conversation.function_outputs,
        snapshot.conversation.text_bytes,
        snapshot.conversation.tool_output_bytes,
        snapshot.attachments.total,
        snapshot.attachments.active,
        snapshot.attachments.removed,
        snapshot.attachments.unsupported,
        snapshot.attachments.stored_bytes,
        snapshot.attachments.redactions,
        snapshot.metrics.tool_calls,
        snapshot.metrics.tool_successes
            + snapshot.metrics.tool_errors
            + snapshot.metrics.tool_denials
            + snapshot.metrics.tool_cancellations,
        snapshot.metrics.receipt_stub_hits + snapshot.metrics.negative_receipt_hits,
        snapshot.metrics.spill_writes,
        snapshot.metrics.spill_reads,
        snapshot.metrics.budget_denials,
        snapshot.metrics.subagent_calls,
        snapshot.metrics.subagent_failures,
        snapshot.metrics.subagent_tool_calls,
        snapshot.metrics.subagent_bytes_read,
        snapshot.metrics.subagent_files_scanned,
        snapshot.metrics.subagent_model_output_bytes,
        snapshot.metrics.subagent_budget_denials,
        format_request_estimate("transmitted_request", &snapshot.transmitted_request),
        format_request_estimate("local_full_history", &snapshot.full_history_request),
    )
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

pub(crate) fn format_reviewer_command(entries: &[ReviewerAuditEntry], now: SystemTime) -> String {
    if entries.is_empty() {
        return "AI reviewer audit\nno auto-decisions recorded this session.".to_string();
    }
    let mut lines: Vec<String> = Vec::with_capacity(entries.len() + 1);
    lines.push(format!(
        "AI reviewer audit\n{} recent decision(s); newest first.",
        entries.len()
    ));
    for entry in entries.iter().rev() {
        let age = format_audit_age(now, entry.recorded_at);
        lines.push(format!(
            "{age} turn={turn} {verdict} {capability} {tool} target={target}\n    reason: {reason}",
            age = age,
            turn = entry.turn_id,
            verdict = entry.verdict.as_str(),
            capability = entry.capability.as_str(),
            tool = entry.tool_name,
            target = entry.target,
            reason = entry.reason,
        ));
    }
    lines.join("\n")
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
