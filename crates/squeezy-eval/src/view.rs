//! `squeezy-eval view` — render a run as a chronological narrative.
//!
//! `trace.jsonl` carries the truth but each event sits on its own
//! line; assistant deltas are split across hundreds of records and tool
//! calls are interleaved with snapshots. A human reviewer has to mentally
//! reconstruct the timeline. This module reads the trace + frames +
//! findings of a single run and emits a chronological markdown transcript
//! a user can read top-to-bottom.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde_json::Value;
use squeezy_tools::human_label_for_call;

use crate::capture::{EvalEvent, EvalEventKind};
use crate::driver::EvalError;
use crate::findings::Finding;

const TOOL_ARG_PREVIEW_CHARS: usize = 120;

pub fn render(run_dir: &Path) -> Result<String, EvalError> {
    let manifest = read_manifest(&run_dir.join("run.json"))?;
    let events = read_events(&run_dir.join("trace.jsonl"))?;
    let findings = read_findings(&run_dir.join("findings.jsonl"))?;

    let mut out = String::new();
    write_header(&mut out, run_dir, &manifest);
    write_findings_summary(&mut out, &findings);
    write_timeline(&mut out, &events);
    write_footer(&mut out, &events);
    Ok(out)
}

fn write_header(out: &mut String, run_dir: &Path, manifest: &Value) {
    let _ = writeln!(out, "# {}", manifest_str(manifest, &["scenario", "title"]));
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "- **Scenario:** `{}` ({})",
        manifest_str(manifest, &["scenario", "id"]),
        manifest_str(manifest, &["scenario", "path"])
    );
    let _ = writeln!(out, "- **Run:** `{}`", run_dir.display());
    let workspace_kind = manifest_str(manifest, &["workspace", "kind"]);
    let _ = match workspace_kind.as_str() {
        "github" => writeln!(
            out,
            "- **Workspace:** {} @ {}",
            manifest_str(manifest, &["workspace", "repo"]),
            short(&manifest_str(manifest, &["workspace", "sha"]), 12)
        ),
        _ => writeln!(
            out,
            "- **Workspace:** {} ({})",
            manifest_str(manifest, &["workspace", "path"]),
            workspace_kind
        ),
    };
    let _ = writeln!(
        out,
        "- **Provider/model:** {} / {}",
        manifest_str(manifest, &["provider"]),
        manifest_str(manifest, &["model"])
    );
    let _ = writeln!(
        out,
        "- **Cost:** {}",
        manifest_str(manifest, &["totals", "cost_display"])
    );
    let _ = writeln!(
        out,
        "- **Events:** {} • **Frames:** {} • **Findings:** {}",
        manifest_str(manifest, &["totals", "trace_events"]),
        manifest_str(manifest, &["totals", "frames"]),
        manifest_str(manifest, &["totals", "findings"])
    );
    let _ = writeln!(out);
}

fn write_findings_summary(out: &mut String, findings: &[Finding]) {
    if findings.is_empty() {
        return;
    }
    let _ = writeln!(out, "## Findings");
    let _ = writeln!(out);
    let mut grouped: BTreeMap<&str, usize> = BTreeMap::new();
    for f in findings {
        *grouped.entry(f.rule_id.as_str()).or_default() += 1;
    }
    for (rule, count) in grouped {
        let _ = writeln!(out, "- `{rule}` × {count}");
    }
    let _ = writeln!(out);
}

fn write_timeline(out: &mut String, events: &[EvalEvent]) {
    let _ = writeln!(out, "## Timeline");
    let _ = writeln!(out);

    // Group consecutive assistant_delta events per turn so the narrative
    // reads as paragraphs, not character streams.
    let mut current_turn: Option<String> = None;
    let mut assistant_buf = String::new();
    let mut turn_start_ts: BTreeMap<String, u64> = BTreeMap::new();

    let flush_assistant = |buf: &mut String, out: &mut String| {
        if buf.is_empty() {
            return;
        }
        let _ = writeln!(out, "**assistant:**");
        let _ = writeln!(out);
        for line in buf.lines() {
            let _ = writeln!(out, "> {line}");
        }
        let _ = writeln!(out);
        buf.clear();
    };

    for event in events {
        let turn_label = event
            .turn_id
            .as_deref()
            .map(short_turn)
            .unwrap_or_else(|| "—".into());
        // Only treat Some→different-Some as a real turn transition. Events
        // with no turn_id (background snapshots) shouldn't re-print the
        // header.
        if let Some(t) = &event.turn_id
            && current_turn.as_ref() != Some(t)
        {
            flush_assistant(&mut assistant_buf, out);
            let _ = writeln!(out, "### Turn {}", short_turn(t));
            let _ = writeln!(out);
            turn_start_ts.entry(t.clone()).or_insert(event.ts_unix_ms);
            current_turn = Some(t.clone());
        }
        match &event.kind {
            EvalEventKind::UserMessage { text } => {
                let _ = writeln!(out, "**user:** {}", trim_oneline(text, 240));
                let _ = writeln!(out);
            }
            EvalEventKind::AssistantDelta { delta } => {
                assistant_buf.push_str(delta);
            }
            EvalEventKind::TurnStarted => {
                let _ = writeln!(out, "_(turn started)_");
                let _ = writeln!(out);
            }
            EvalEventKind::TurnCompleted { cost, metrics } => {
                flush_assistant(&mut assistant_buf, out);
                let input = cost
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let output = cost
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let tool_calls = metrics
                    .get("tool_calls")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let elapsed = turn_start_ts
                    .get(current_turn.as_deref().unwrap_or(""))
                    .map(|start| event.ts_unix_ms.saturating_sub(*start))
                    .unwrap_or(0);
                let _ = writeln!(
                    out,
                    "_(turn complete · {} tool call(s) · in={input} out={output} · {}ms)_",
                    tool_calls, elapsed
                );
                let _ = writeln!(out);
            }
            EvalEventKind::TurnFailed { error } => {
                flush_assistant(&mut assistant_buf, out);
                let _ = writeln!(
                    out,
                    "**🚨 turn failed:** `{}`",
                    trim_oneline(error, 200).replace('`', "ʼ")
                );
                let _ = writeln!(out);
            }
            EvalEventKind::TurnCancelled => {
                flush_assistant(&mut assistant_buf, out);
                let _ = writeln!(out, "_(turn cancelled)_");
                let _ = writeln!(out);
            }
            EvalEventKind::ToolCallStarted { call, origin } => {
                let name = call.get("name").and_then(Value::as_str).unwrap_or("?");
                let label = call
                    .get("arguments")
                    .map(|args| human_label_for_call(name, args))
                    .unwrap_or_else(|| name.to_string());
                let _ = writeln!(
                    out,
                    "{icon} **{label}**",
                    icon = icon_for_origin(origin),
                    label = trim_oneline(&label, TOOL_ARG_PREVIEW_CHARS).replace('`', "ʼ")
                );
            }
            EvalEventKind::ToolCallCompleted { result } => {
                let name = result
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let status = result.get("status").and_then(Value::as_str).unwrap_or("?");
                let bytes = result
                    .get("cost_hint")
                    .and_then(|v| v.get("output_bytes"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let icon = match status {
                    "Success" => "  ↳ ✅",
                    "Error" => "  ↳ ❌",
                    "Cancelled" => "  ↳ ⏹",
                    "Denied" => "  ↳ ⛔",
                    _ => "  ↳",
                };
                let _ = writeln!(out, "{icon} {name} {status} ({bytes}B out)");
                let _ = writeln!(out);
            }
            EvalEventKind::ToolCallQueued { .. } => { /* noise */ }
            EvalEventKind::Approval { request, decision } => {
                let tool = request.get("tool").and_then(Value::as_str).unwrap_or("?");
                let _ = writeln!(out, "🛂 **approval:** `{tool}` → `{decision}`");
                let _ = writeln!(out);
            }
            EvalEventKind::SlashCommand { command } => {
                let _ = writeln!(out, "⌘ **slash:** `{command}`");
                let _ = writeln!(out);
            }
            EvalEventKind::ContextCompacted { .. } => {
                let _ = writeln!(out, "🗜 **/compact applied**");
                let _ = writeln!(out);
            }
            EvalEventKind::Finding {
                rule_id,
                severity,
                summary,
            } => {
                let _ = writeln!(
                    out,
                    "🔎 **finding [{severity}] `{rule_id}`** {}",
                    trim_oneline(summary, 200)
                );
                let _ = writeln!(out);
            }
            EvalEventKind::ActionStep { action, status } => {
                let kind = action
                    .get("kind")
                    .or_else(|| action.get("action"))
                    .and_then(Value::as_str)
                    .unwrap_or("action");
                // Skip noisy `prompt` send events — the UserMessage that
                // follows is the meaningful signal.
                if kind == "prompt" {
                    continue;
                }
                let _ = writeln!(out, "➤ **{kind}:** {}", trim_oneline(status, 200));
                let _ = writeln!(out);
            }
            EvalEventKind::CostUpdate {
                tool_count,
                input_tokens,
                micro_usd,
            } => {
                let _ = writeln!(
                    out,
                    "💰 _running this turn:_ {} in · {} (after {} tool(s))",
                    format_token_count(*input_tokens),
                    format_micro_usd(*micro_usd),
                    tool_count
                );
                let _ = writeln!(out);
            }
            EvalEventKind::ToolProgress { .. } => {
                // Heartbeats are noise in a post-run timeline; the
                // ToolCallCompleted event carries the final duration.
            }
            EvalEventKind::TaskStateUpdated { .. }
            | EvalEventKind::SubagentEvent { .. }
            | EvalEventKind::Snapshot { .. }
            | EvalEventKind::PerfSample { .. } => {
                // Quiet noise; full detail available in trace.jsonl.
            }
        }
        let _ = turn_label;
    }
    // Final flush in case a stream ended mid-assistant.
    flush_assistant(&mut assistant_buf, out);
}

fn write_footer(out: &mut String, events: &[EvalEvent]) {
    let total = events.len();
    let _ = writeln!(out, "---");
    let _ = writeln!(
        out,
        "Rendered from {total} trace events. Use `trace.jsonl` / \
         `frames.jsonl` for full machine-readable detail."
    );
}

fn read_manifest(path: &Path) -> Result<Value, EvalError> {
    let bytes =
        std::fs::read(path).map_err(|err| EvalError::Io(format!("read {path:?}: {err}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|err| EvalError::Internal(format!("parse manifest: {err}")))
}

fn read_events(path: &Path) -> Result<Vec<EvalEvent>, EvalError> {
    use std::io::{BufRead, BufReader};
    let file =
        std::fs::File::open(path).map_err(|err| EvalError::Io(format!("open {path:?}: {err}")))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|err| EvalError::Io(format!("read {path:?}: {err}")))?;
        if line.trim().is_empty() {
            continue;
        }
        let event: EvalEvent = serde_json::from_str(&line)
            .map_err(|err| EvalError::Internal(format!("parse event: {err}")))?;
        out.push(event);
    }
    Ok(out)
}

fn read_findings(path: &Path) -> Result<Vec<Finding>, EvalError> {
    if !path.exists() {
        return Ok(vec![]);
    }
    use std::io::{BufRead, BufReader};
    let file =
        std::fs::File::open(path).map_err(|err| EvalError::Io(format!("open {path:?}: {err}")))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|err| EvalError::Io(format!("read {path:?}: {err}")))?;
        if line.trim().is_empty() {
            continue;
        }
        let f: Finding = serde_json::from_str(&line)
            .map_err(|err| EvalError::Internal(format!("parse finding: {err}")))?;
        out.push(f);
    }
    Ok(out)
}

fn manifest_str(v: &Value, path: &[&str]) -> String {
    let mut current = v;
    for key in path {
        match current.get(*key) {
            Some(next) => current = next,
            None => return "(missing)".into(),
        }
    }
    match current {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

fn short(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn short_turn(t: &str) -> String {
    // Render `TurnId(1)` as `1` for readability.
    if let Some(rest) = t.strip_prefix("TurnId(")
        && let Some(rest) = rest.strip_suffix(')')
    {
        return rest.to_string();
    }
    t.to_string()
}

fn icon_for_origin(origin: &str) -> &'static str {
    match origin {
        "planner" => "🧭",
        "subagent" => "🤖",
        _ => "🔧",
    }
}

fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.0}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_micro_usd(micro: u64) -> String {
    let dollars = micro as f64 / 1_000_000.0;
    if dollars < 0.01 {
        format!("${dollars:.4}")
    } else {
        format!("${dollars:.3}")
    }
}

fn trim_oneline(s: &str, max: usize) -> String {
    let collapsed: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    let trimmed = collapsed.trim();
    if trimmed.chars().count() <= max {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(max).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
